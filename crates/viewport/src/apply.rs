// SPDX-License-Identifier: GPL-3.0-or-later
//
// What each IPC message does. Ports the handler bodies in src/ipc.c.
//
// The defining property of this file is how little geometry is in it: the shell
// computed every rectangle, and the compositor's job is to put the surface
// where it was told and otherwise stay out of the way.

use smithay::output::{Mode as OutputMode, Output, Scale};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::utils::{Transform as SmithayTransform, SERIAL_COUNTER};

use viewport_ipc::request::{ModeRequest, OutputConfigure};
use viewport_ipc::{Event, Request, Transform};

use crate::session;
use crate::state::ViewportState;
use crate::views::{Configured, NO_VIEW};

/// How many bindings the `bind.add` message may grow the list to.
///
/// The defaults and a hand-written keymap are well under this; it exists so a
/// page that only ever adds cannot make every key press walk an unbounded list.
const MAX_RUNTIME_BINDINGS: usize = 1024;

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
/// one. `shell.exec` runs a line on the host; the four `input.*` requests
/// drive the seat as though a hand had — keys, buttons, motion and the wheel —
/// which behind a lock screen means into whatever box the locker drew;
/// `bind.add` installs a chord that the key path will refuse while locked but
/// that survives past the unlock; and quitting takes the lock screen down with
/// the compositor — a frozen session is logind's to recover, not the socket's.
///
/// `power.action`'s suspend, reboot and poweroff are deliberately *not* here,
/// though they end the session further than `quit` does. A lock screen must
/// offer the user those: the machine is theirs to walk away from or switch
/// off, and a locker that traps them on it is worse than one that lets a
/// trusted sender reach logind — the same authority the lid switch uses, and
/// the same three actions it takes. Quitting is refused not because ending
/// the session is forbidden but because this compositor owns its own
/// lifetime: pulling it down takes the lock screen with it and leaves the
/// machine unlocked and half-dead, which is exactly what the lock is for.
fn acts_while_locked(request: &Request) -> bool {
    matches!(
        request,
        Request::ShellExec { .. }
            | Request::InputKey { .. }
            | Request::InputButton { .. }
            | Request::InputPointer { .. }
            | Request::InputScroll { .. }
            | Request::BindAdd { .. }
            | Request::AiLogin { .. }
            | Request::Quit
    )
}

/// Whether carrying this request out only asks for state, changing nothing.
///
/// The control socket is shared by the shell, by scripts and — through a
/// page's own code — by whatever that page can reach. A query is safe to
/// answer for any same-user client. Everything else acts on the session, the
/// desktop or the host, and is held to the sender check in `apply`.
///
/// `OskKey` is deliberately absent from `acts_while_locked`: a synthetic key
/// still has to reach the lock screen, so its refusal happens per key in
/// `inject_key`.
fn reads_only(request: &Request) -> bool {
    // `SessionQuery` looks like a read but is not one: while locked it
    // withdraws the lock screen state and asks the page to draw the lock
    // again, so an untrusted caller could make that happen in a loop. It is
    // privileged like the rest of the session messages; the shell and the
    // CLI are trusted.
    matches!(
        request,
        Request::ViewQuery | Request::ClipboardQuery | Request::NotificationList
    )
}

pub fn apply(state: &mut ViewportState, request: Request) {
    // Who is allowed to ask for what, before anything in here is allowed to
    // touch the session.
    //
    // The shell runs with the same user id as the compositor — it has to, to
    // write the config — so uid alone cannot tell a page that escaped its
    // browser from the desktop. Only connections the compositor itself
    // started are trusted; client zero is an in-process apply (the config
    // file, the watchdog) and is the compositor. Everyone else may ask
    // questions, and no one else may act.
    if state.dispatch_client != 0
        && !reads_only(&request)
        && !state.client_is_trusted(state.dispatch_client)
    {
        tracing::warn!(
            "refusing a privileged request from untrusted client {}",
            state.dispatch_client
        );
        reject(
            state,
            "ipc",
            "this client is not allowed to send that request",
        );
        return;
    }

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
                    // The renderer draws from the space and no client commits
                    // for being shown again: the exact mirror of the unmap
                    // below, and needing the same mark or the reopened
                    // window stays invisible until something else redraws.
                    state.needs_render = true;
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
            set_view_fullscreen(state, id, fullscreen);
        }

        Request::ViewMaximized { id, maximized } => {
            set_view_maximized(state, id, maximized);
        }

        Request::ViewMinimized { id, minimized } => {
            set_view_minimized(state, id, minimized);
        }

        Request::ViewFocus { id } => focus_view(state, id),

        Request::ViewClose { id } => {
            // An X11 window has no xdg toplevel, so `toplevel()` alone drops
            // every XWayland client. The compositor's own Close binding gets
            // this right in `input.rs`; a taskbar's close button and
            // `viewport msg -t view.close` come through here.
            if let Some(view) = state.views.get(id) {
                if let Some(toplevel) = view.window.toplevel() {
                    toplevel.send_close();
                } else if let Some(x11) = view.window.x11_surface() {
                    if let Err(e) = x11.close() {
                        tracing::warn!("could not close X11 view {id}: {e}");
                    }
                }
            }
        }

        Request::ViewOpacity { id, opacity } => {
            if let Some(view) = state.views.get_mut(id) {
                let opacity = opacity.clamp(0.0, 1.0) as f32;
                if view.opacity != opacity {
                    view.opacity = opacity;
                    // A fade to zero is a debt: the shell must raise it again.
                    // If it never does, the window is a see-through hole with
                    // its frame around it — see `settle_see_through_views`.
                    if opacity <= 0.001 {
                        state.see_through_pending = true;
                    }
                    // The drawn alpha of the window changes and nothing
                    // commits with it — no client knows its window faded — so
                    // this is the only mark the change gets. Without it the
                    // fade lands whenever something else next draws.
                    state.needs_render = true;
                }
            }
        }

        Request::ViewOpacityRule { id, opacity } => {
            if let Some(view) = state.views.get_mut(id) {
                let opacity = if opacity.is_finite() {
                    opacity.max(0.0) as f32
                } else {
                    1.0
                };
                if view.rule_opacity != opacity {
                    view.rule_opacity = opacity;
                    state.needs_render = true;
                }
            }
        }

        Request::ViewCapture { id, capture } => {
            let Some(view) = state.views.get_mut(id) else {
                return;
            };
            tracing::debug!("view {id}: capture policy set to {capture}");
            // An explicit answer, so identity changes stop re-deriving the
            // conservative default over the top of it. See `capture_resolved`.
            view.capture_resolved = true;
            if view.capture_allowed != capture {
                view.capture_allowed = capture;
                // Capture is serviced from a render pass. Wake an idle backend
                // so the changed policy reaches any frame waiting on it.
                state.needs_render = true;
            }
        }

        Request::ViewTearing { id, tearing } => {
            if let Some(view) = state.views.get_mut(id) {
                view.rule_tearing = tearing;
            }
        }

        Request::ViewIdleInhibit { id, inhibit } => {
            let Some(view) = state.views.get_mut(id) else {
                return;
            };
            if view.rule_idle_inhibit != inhibit {
                view.rule_idle_inhibit = inhibit;
                // The inhibitor list is what the idle timer reads, so it has
                // to be recomputed the moment a rule changes it.
                state.refresh_idle_inhibit();
            }
        }

        Request::ViewDecorations { id, decorate } => {
            let changed = match state.views.get_mut(id) {
                Some(view) if view.rule_decorate != decorate => {
                    view.rule_decorate = decorate;
                    true
                }
                _ => false,
            };
            if changed {
                // The client must be told the moment the rule moves, not at
                // the next new_decoration.
                state.refresh_decoration(id);
            }
        }

        Request::ViewQuery => {
            state.notify_config();
            state.notify(&viewport_ipc::Event::ScreencastActive {
                active: !state.casts.is_empty(),
            });
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
            // The chooser is a dialog: it is there to be clicked, and it is
            // drawn over whatever it is asking about — so it blurs that too.
            // It takes the keyboard on a press like any other overlay, so the
            // keep-focus list has nothing to carry here.
            state.set_shell_overlays(rects.clone(), rects.clone(), rects, Vec::new());
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
                        rect.blur,
                        rect.keep_focus,
                    )
                })
                .collect();
            // Everything the shell floats is drawn above the windows; only the
            // ones that did not ask to be seen through take the pointer.
            let hits = placed
                .iter()
                .filter(|(_, passthrough, _, _)| !passthrough)
                .map(|(rect, _, _, _)| *rect)
                .collect();
            let blur = placed
                .iter()
                .filter(|(_, _, blur, _)| *blur)
                .map(|(rect, _, _, _)| *rect)
                .collect();
            // And of the ones that take the press, the ones that leave the
            // keyboard's focus alone — see `OverlayRect::keep_focus`. Only
            // those: a press through a `passthrough` rectangle lands on the
            // window behind it, and that window is where the keyboard
            // belongs.
            let keep_focus = placed
                .iter()
                .filter(|(_, passthrough, _, keep_focus)| !passthrough && *keep_focus)
                .map(|(rect, _, _, _)| *rect)
                .collect();
            let all = placed.into_iter().map(|(rect, _, _, _)| rect).collect();
            state.set_shell_overlays(all, hits, blur, keep_focus);
        }

        Request::ShellOverview { active } => {
            state.overview = active;
            // Every click belongs to the shell while the overview is up and to
            // the windows again when it is down, and neither transition
            // involves the pointer moving. Same reason as `set_shell_overlays`.
            let _ = state.refresh_pointer_focus();
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

        Request::SessionLockDrawn { generation } => {
            // The blanket sender check above already turned away every
            // socket the compositor does not trust. Kept explicit as well:
            // this is the one request whose whole meaning is "the lock
            // screen is really mine", and it must still hold if the
            // blanket rule is ever loosened.
            if state.dispatch_client != 0 && !state.client_is_trusted(state.dispatch_client) {
                reject(
                    state,
                    "session.lock.drawn",
                    "this sender may not report the lock screen",
                );
                return;
            }
            state.lock_screen_drawn(generation);
        }

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

        Request::OutputActive { name } => {
            // A real head, not just a string: `active_output` is where a new
            // window opens and which output layer surfaces default to, and a
            // name nothing answers to leaves `ViewAdded` naming a monitor
            // that does not exist. `any_output_by_name` rather than
            // `output_by_name`, because a mirror sink is a real head that is
            // deliberately absent from the logical `Space`.
            if state.any_output_by_name(&name).is_none() {
                reject(state, "output.active", "no such output");
            } else {
                state.active_output = Some(name);
            }
        }

        Request::GestureCapture { gestures } => {
            // Whole replacement, with malformed zero-finger entries and
            // duplicates made harmless. The active sequence stays frozen.
            let mut gestures: Vec<_> = gestures
                .into_iter()
                .filter(|gesture| gesture.fingers > 0)
                .collect();
            gestures.sort_by_key(|gesture| (gesture.kind as u8, gesture.fingers));
            gestures.dedup();
            state.live_gestures = gestures;
        }

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
            crate::input::spawn_with_env(&command, &state.child_display_env());
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
                state.status_tick_with_osd(Some(if node == crate::status::SINK {
                    viewport_ipc::event::StatusOsd::Volume
                } else {
                    viewport_ipc::event::StatusOsd::Microphone
                }));
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
            //
            // Bounded: every one is walked on every key press, so a page that
            // only ever adds grows both the list and the per-keystroke cost for
            // the life of the session. A chord that replaces an existing one is
            // always allowed — that is how a runtime bind is taken back.
            let replaces =
                crate::binding::parse(&format!("{chord}={action}")).is_some_and(|binding| {
                    state
                        .bindings
                        .iter()
                        .any(|existing| same_chord(existing, &binding))
                });
            if !replaces && state.bindings.len() >= MAX_RUNTIME_BINDINGS {
                reject(state, "bind.add", "too many runtime bindings");
            } else if !install_binding(&mut state.bindings, &chord, &action) {
                reject(state, "bind.add", &format!("{chord}={action}"));
            }
        }

        // Driving the pointer from the socket. The same calls the libinput
        // path makes, in the same order, so a scripted click and a real one
        // are the same event by the time anything sees it.
        Request::InputPointer { x, y } => state.inject_pointer(x, y),

        Request::InputButton { button, pressed } => state.inject_button(button, pressed),

        Request::InputKey { keycode, pressed } => state.inject_key(keycode, pressed),

        Request::InputScroll { dx, dy } => state.inject_scroll(dx, dy),

        Request::ConfigGaps {
            inner,
            outer,
            smart,
        } => {
            // Only the fields given change; the others keep whatever they are.
            // This is how a keybinding on `inner` does not clobber an `outer`
            // set from the config file or an earlier IPC call.
            //
            // The whole message is validated before anything is changed, so a
            // refusal leaves every field alone — the shape ConfigWallpaper
            // already takes. Validating and applying field by field left the
            // valid half applied whenever a later field was refused, and the
            // shell and the compositor diverged from that point on.
            if let Some(v) = inner {
                if v < 0 {
                    reject(state, "config.gaps", &format!("inner {v}"));
                    return;
                }
            }
            if let Some(v) = outer {
                if v < 0 {
                    reject(state, "config.gaps", &format!("outer {v}"));
                    return;
                }
            }
            let current = state
                .config
                .gaps
                .get_or_insert_with(viewport_ipc::event::Gaps::default);
            let mut changed = false;
            if let Some(v) = inner {
                current.inner = Some(v);
                changed = true;
            }
            if let Some(v) = outer {
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
            // Validated as a whole before anything changes, like ConfigGaps
            // above: a refused message must not leave its valid half applied.
            if let Some(v) = radius {
                if v < 0 {
                    reject(state, "config.border", &format!("radius {v}"));
                    return;
                }
            }
            if let Some(v) = width {
                if v < 0 {
                    reject(state, "config.border", &format!("width {v}"));
                    return;
                }
            }
            let current = state
                .config
                .border
                .get_or_insert_with(viewport_ipc::event::Border::default);
            let mut changed = false;
            if let Some(v) = radius {
                current.radius = Some(v);
                changed = true;
            }
            if let Some(v) = width {
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
                        // Detailed in the log, generic to the sender: the
                        // resolved path is an existence oracle for a page
                        // that names paths it cannot otherwise read.
                        tracing::warn!("config.wallpaper: {e:#}");
                        reject(state, "config.wallpaper", "the wallpaper could not be set");
                        return;
                    }
                },
                None => None,
            };
            let mode = match mode.as_deref() {
                Some(mode) => match crate::config::parse_wallpaper_mode(mode) {
                    Ok(mode) => Some(mode),
                    Err(e) => {
                        // The mode error names the modes the compositor
                        // knows; unlike a path it is not a filesystem
                        // oracle, so the sender keeps the useful message.
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

        Request::ConfigCursor {
            hide_after_ms,
            hide_on_key_press,
            hide_on_touch,
            theme,
            size,
            follow_mouse,
            follow_mouse_threshold,
        } => {
            // Validated as a whole before anything changes, like ConfigGaps:
            // a negative deadline, a zero size or a negative threshold takes
            // the whole message with it.
            if let Some(v) = hide_after_ms {
                if v < 0 {
                    reject(state, "config.cursor", &format!("hide_after_ms {v}"));
                    return;
                }
            }
            if size == Some(0) {
                reject(state, "config.cursor", "size 0");
                return;
            }
            if let Some(v) = follow_mouse_threshold {
                if v < 0.0 {
                    reject(
                        state,
                        "config.cursor",
                        &format!("follow_mouse_threshold {v}"),
                    );
                    return;
                }
            }
            // A bare message toggles the deadline, on the same terms as
            // `config.dark_mode` — a keybinding wants to flip a switch without
            // knowing the number. On becomes never-hide (0) and off becomes
            // the shipped 2000ms. A message naming any other field leaves the
            // deadline alone, which is the panel's case.
            let hide_after_ms = match hide_after_ms {
                Some(v) => Some(v),
                None if hide_on_key_press.is_none()
                    && hide_on_touch.is_none()
                    && theme.is_none()
                    && size.is_none()
                    && follow_mouse.is_none()
                    && follow_mouse_threshold.is_none() =>
                {
                    Some(if state.cursor_hide.after().is_some() {
                        0
                    } else {
                        2000
                    })
                }
                None => None,
            };
            let mut changed = false;
            let mut needs_render = false;
            // The compositor-side half first, before the Config copy is
            // borrowed below: the theme reloads its images, which is a full
            // borrow of the state. Both leave their own damage behind.
            if theme.is_some() || size.is_some() {
                let theme = theme
                    .clone()
                    .unwrap_or_else(|| state.cursor_theme.name().to_owned());
                let size = size.unwrap_or_else(|| state.cursor_theme.size());
                state.apply_cursor_theme(theme, size, state.cursor_theme.hyprcursor_enabled());
                changed = true;
            }
            if let Some(on) = follow_mouse {
                // The threshold resets when the feature is toggled, so the
                // first motion after enabling does not have to re-cross the
                // old distance — the same rule the reload path follows.
                if state.follow_mouse != on {
                    state.follow_mouse = on;
                    state.follow_mouse_pos = None;
                    changed = true;
                }
            }
            if let Some(v) = follow_mouse_threshold {
                if state.follow_mouse_threshold != v {
                    state.follow_mouse_threshold = v;
                    changed = true;
                }
            }
            let current = state
                .config
                .cursor
                .get_or_insert_with(viewport_ipc::event::Cursor::default);
            if let Some(v) = hide_after_ms {
                // `set_after_ms` returns whether a hidden cursor had to be
                // drawn again — the deadline that hid it is gone, so nothing
                // else would. That is damage the caller must paint.
                needs_render |= state.cursor_hide.set_after_ms(Some(v));
                current.hide_after_ms = Some(v);
                changed = true;
            }
            if let Some(on) = hide_on_key_press {
                needs_render |= state.cursor_hide.set_on_key_press(on);
                current.hide_on_key_press = Some(on);
                changed = true;
            }
            if let Some(on) = hide_on_touch {
                needs_render |= state.cursor_hide.set_on_touch(on);
                current.hide_on_touch = Some(on);
                changed = true;
            }
            if let Some(v) = theme {
                current.theme = Some(v);
            }
            if let Some(v) = size {
                current.size = Some(v);
            }
            if let Some(on) = follow_mouse {
                current.follow_mouse = Some(on);
            }
            if let Some(v) = follow_mouse_threshold {
                current.follow_mouse_threshold = Some(v);
            }
            if changed {
                // The shell draws nothing from this — the cursor is the
                // compositor's — but the Config event carries it so the panel
                // shows the switch where it really is and `config.save` writes
                // what is on screen. Arming starts a countdown that was off;
                // turning it off leaves an armed timer that fires once into
                // nothing, which is what the reload path already does.
                if needs_render {
                    state.needs_render = true;
                }
                state.notify_config();
                state.arm_cursor_hide();
            }
        }

        Request::ConfigMotion {
            enabled,
            duration,
            slow,
            ease,
        } => {
            // Validated as a whole before anything changes, like ConfigGaps:
            // a negative or non-finite pace takes the whole message with it.
            for (name, value) in [("duration", duration), ("slow", slow)] {
                if let Some(v) = value {
                    if !(v.is_finite() && v >= 0.0) {
                        reject(state, "config.motion", &format!("{name} {v}"));
                        return;
                    }
                }
            }
            let current = state
                .config
                .motion
                .get_or_insert_with(viewport_ipc::event::Motion::default);
            let mut changed = false;
            if let Some(v) = enabled {
                current.enabled = Some(v);
                changed = true;
            }
            if let Some(v) = duration {
                current.duration = Some(v);
                changed = true;
            }
            if let Some(v) = slow {
                current.slow = Some(v);
                changed = true;
            }
            if let Some(v) = ease {
                current.ease = Some(v);
                changed = true;
            }
            if changed {
                // The shell owns every transition and lands the pace on
                // `--anim`, `--anim-slow` and `--ease`; the page repaints
                // itself, so there is no damage of the compositor's own to
                // paint.
                state.notify_config();
            }
        }

        Request::ConfigGroup { auto_group } => {
            // One boolean and nothing to validate: the shell decides what a
            // group is, and the compositor has no groups at all.
            if auto_group.is_some() {
                let current = state
                    .config
                    .group
                    .get_or_insert_with(viewport_ipc::event::Group::default);
                current.auto_group = auto_group;
                state.notify_config();
            }
        }

        Request::ConfigClock {
            locale,
            hour12,
            format,
        } => {
            // Forwarded whole, and only when the message names one of them:
            // the shell's own answer to an absent block is the locale the
            // engine runs under, which is not a value this side could write
            // down. Nothing is validated beyond the type — a tag the engine
            // has no data for is the engine's business.
            if locale.is_some() || hour12.is_some() || format.is_some() {
                let current = state
                    .config
                    .clock
                    .get_or_insert_with(viewport_ipc::event::Clock::default);
                if let Some(v) = locale {
                    current.locale = Some(v);
                }
                if let Some(v) = hour12 {
                    current.hour12 = Some(v);
                }
                if let Some(v) = format {
                    current.format = Some(v);
                }
                state.notify_config();
            }
        }

        Request::ConfigBar { mode } => {
            if let Some(mode) = mode {
                state.config.bar = Some(mode);
                state.notify_config();
            }
        }

        Request::ConfigBarBlur { enabled } => {
            // Absent toggles, on the same terms as `config.dark_mode`.
            let want = enabled.unwrap_or(!state.config.bar_blur);
            if want != state.config.bar_blur {
                state.config.bar_blur = want;
                // The blur is a compositor framebuffer effect under the
                // floating bar: without a new frame it keeps drawing for a
                // bar that asked to be flat.
                state.needs_render = true;
                state.notify_config();
            }
        }

        Request::ConfigBarWidgets { widgets } => {
            if let Some(widgets) = widgets {
                // Parsed into the file's own shape so the sampler prices it
                // the same way a reload does; a list that does not parse is
                // refused whole. The override keeps its precedence: what
                // is in `bar_items_config` is still on the bar, so the
                // additions wait for it to go.
                let widgets: Vec<crate::config::BarWidgetConfig> =
                    match serde_json::from_value(widgets) {
                        Ok(widgets) => widgets,
                        Err(e) => {
                            reject(state, "config.bar_widgets", &format!("{e}"));
                            return;
                        }
                    };
                let items = state.bar_items_config.clone();
                state.apply_bar_config(&widgets, items.as_deref());
                state.notify_config();
            }
        }

        Request::ConfigBarItems { items } => {
            if let Some(items) = items {
                let items: Vec<crate::config::BarItemConfig> = match serde_json::from_value(items) {
                    Ok(items) => items,
                    Err(e) => {
                        reject(state, "config.bar_items", &format!("{e}"));
                        return;
                    }
                };
                // The widgets are superseded — the override draws exactly
                // what it lists.
                state.apply_bar_config(&[], Some(&items));
                state.notify_config();
            }
        }

        Request::ConfigLogo { enabled } => {
            // Absent toggles, on the same terms as `config.dark_mode`.
            let want = enabled.unwrap_or(!state.config.logo);
            if want != state.config.logo {
                state.config.logo = want;
                state.notify_config();
            }
        }

        Request::ConfigTutorial { enabled } => {
            // Absent toggles, on the same terms as `config.dark_mode`.
            let want = enabled.unwrap_or(!state.config.tutorial);
            if want != state.config.tutorial {
                state.config.tutorial = want;
                state.notify_config();
            }
        }

        Request::ConfigTilingMode { mode } => {
            if let Some(mode) = mode {
                // Checked here rather than in the shell, the same place the
                // file is checked: an unknown mode would otherwise reach
                // the shell, fail to match any arrangement, and leave the
                // tree manual with nothing said.
                const MODES: [&str; 5] = ["manual", "master-stack", "spiral", "bsp", "grid"];
                if !MODES.contains(&mode.as_str()) {
                    reject(
                        state,
                        "config.tiling_mode",
                        &format!("unknown mode {mode:?}"),
                    );
                    return;
                }
                state.config.tiling_mode = Some(mode);
                state.notify_config();
            }
        }

        Request::ConfigLayout { layout } => {
            if let Some(layout) = layout {
                // Checked the same way the file is: an unknown name would
                // otherwise leave the shell on the tiling default while the
                // keymap was built for a layout that does not exist.
                let extension = state
                    .config
                    .layout_extensions
                    .iter()
                    .any(|entry| entry.name == layout);
                if !crate::config::BUILTIN_LAYOUTS.contains(&layout.as_str()) && !extension {
                    reject(
                        state,
                        "config.layout",
                        &format!("unknown layout {layout:?}"),
                    );
                    return;
                }
                state.config.layout = layout;
                state.notify_config();
            }
        }

        Request::ConfigLayoutExtensions { extensions } => {
            if let Some(extensions) = extensions {
                let mut extensions = extensions;
                extensions.sort_by(|a, b| a.name.cmp(&b.name));
                // If the layout in use names something no longer registered,
                // it falls back to tiling — the same answer a reload gives a
                // typo.
                let kept = crate::config::BUILTIN_LAYOUTS.contains(&state.config.layout.as_str())
                    || extensions
                        .iter()
                        .any(|entry| entry.name == state.config.layout);
                if !kept {
                    tracing::warn!(
                        "unknown layout {:?}; falling back to tiling after layout_extensions changed",
                        state.config.layout
                    );
                    state.config.layout = "tiling".to_owned();
                }
                state.config.layout_extensions = extensions;
                state.notify_config();
            }
        }

        Request::ConfigFocusCrossesOutputs { enabled } => {
            // Absent toggles, on the same terms as `config.dark_mode`.
            let want = enabled.unwrap_or(!state.config.focus_crosses_outputs);
            if want != state.config.focus_crosses_outputs {
                state.config.focus_crosses_outputs = want;
                state.notify_config();
            }
        }

        Request::ConfigOsk { mode } => {
            if let Some(mode) = mode {
                match crate::config::parse_osk_mode(&mode) {
                    Ok(mode) => {
                        state.osk_mode = mode;
                        state.config.osk = mode.as_str().to_owned();
                        // Recomputed right away, for the reason the reload
                        // path gives: a text-input still enabled under a
                        // turned-off keyboard is exactly what this has to
                        // reach, and waiting for the next commit would leave
                        // it up.
                        state.sync_osk_wanted();
                        state.notify_config();
                    }
                    Err(e) => {
                        reject(state, "config.osk", &e.to_string());
                    }
                }
            }
        }

        Request::ConfigTray { enabled } => {
            // Absent toggles, on the same terms as `config.dark_mode`.
            let want = enabled.unwrap_or(!state.tray_enabled);
            if want != state.tray_enabled {
                state.tray_enabled = want;
                // Applied right away, for the reason the reload path gives:
                // turning it off releases the names and empties the bar, so
                // applications see the tray go away as if the compositor
                // had exited.
                state.tray.set_enabled(want);
                state.config.tray = Some(want);
                state.notify_config();
            }
        }

        Request::ConfigClipboardHistory { limit } => {
            if let Some(limit) = limit {
                // Applied right away, for the reason the reload path gives:
                // lowering it drops the oldest entries there and then, and
                // zero empties the history rather than leaving them behind
                // a setting that says it keeps nothing.
                state.clipboard.set_limit(limit);
                state.config.clipboard_history = Some(limit);
                state.notify_config();
            }
        }

        Request::ConfigIconTheme { theme } => {
            if let Some(theme) = theme {
                state.tray.set_icon_theme(theme.clone());
                state.icon_theme = theme.clone();
                // Applied right away, for the reason the reload path gives:
                // the theme is what somebody reaches for when they install
                // icons and want to see them without restarting, and the
                // caches are the scanner's, so the emptying is a message.
                state.launcher_scan.clear_icons();
                state.config.icon_theme = Some(theme);
                state.notify_config();
            }
        }

        Request::ConfigTheme { value } => {
            if let Some(value) = value {
                // Taken whole, the way the file's `theme` is.
                state.config.theme = Some(value);
                state.notify_config();
            }
        }

        Request::ConfigRules { value } => {
            if let Some(value) = value {
                state.config.rules = Some(value);
                // The same tightening a reload does: a window still awaiting
                // its resolved capture answer loses the capture the new
                // rules deny, and one that already carries an explicit
                // answer — the shell's, or a `view.capture`'s — keeps it.
                let rules = state.config.rules.clone();
                for view in state
                    .views
                    .views_mut()
                    .filter(|view| view.mapped && !view.capture_resolved)
                {
                    if !crate::config::initially_allows_capture(
                        rules.as_ref(),
                        &view.app_id(),
                        &view.title(),
                        view.tag.as_deref(),
                    ) {
                        view.capture_allowed = false;
                    }
                }
                state.notify_config();
            }
        }

        Request::ConfigLayerRules { rules } => {
            if let Some(rules) = rules {
                // Compiled before anything changes, so a set that does not
                // compile leaves the old one standing.
                let configured: Vec<crate::layer::RuleConfig> =
                    match serde_json::from_value(rules.clone()) {
                        Ok(configured) => configured,
                        Err(e) => {
                            reject(state, "config.layer_rules", &format!("{e}"));
                            return;
                        }
                    };
                match crate::layer::Rules::compile(configured) {
                    Ok(compiled) => {
                        state.layer_rules = compiled;
                        state.config.layer_rules = Some(rules);
                        state.refresh_layer_policies();
                        state.needs_render = true;
                        state.notify_config();
                    }
                    Err(e) => {
                        reject(state, "config.layer_rules", &format!("{e}"));
                    }
                }
            }
        }

        Request::ConfigDecorations { mode } => {
            if let Some(mode) = mode {
                // "client" hands the frame back; anything else, including a
                // value nobody recognises, keeps it here — what the file
                // says, applied the same way. The advertised default moves
                // with it, so a client probing the manager and one asking
                // per-surface still get the same answer.
                let server = mode != "client";
                if server != state.server_decorations {
                    state.server_decorations = server;
                    use smithay::reexports::wayland_protocols_misc::server_decoration::server::org_kde_kwin_server_decoration_manager::Mode as KdeDefaultMode;
                    state.kde_decoration_state.set_default_mode(if server {
                        KdeDefaultMode::Server
                    } else {
                        KdeDefaultMode::Client
                    });
                    state.needs_render = true;
                }
                state.config.decorations = Some(mode);
                state.notify_config();
            }
        }

        Request::ConfigWorkspaces { rules } => {
            if let Some(rules) = rules {
                let rules: std::collections::HashMap<u32, crate::config::WorkspaceConfig> =
                    match serde_json::from_value(rules) {
                        Ok(rules) => rules,
                        Err(e) => {
                            reject(state, "config.workspaces", &format!("{e}"));
                            return;
                        }
                    };
                // Validated the same way the file is — one bad workspace
                // degrades to the global inheritance, not the whole
                // message.
                state.config.workspaces = state.convert_workspace_rules(rules);
                state.notify_config();
            }
        }

        Request::ConfigOpacity {
            active,
            inactive,
            fullscreen,
        } => {
            // Validated as a whole before anything changes, like ConfigGaps:
            // a multiplier that is not a finite non-negative number takes
            // the whole message with it.
            for (name, value) in [
                ("active", active),
                ("inactive", inactive),
                ("fullscreen", fullscreen),
            ] {
                if let Some(v) = value {
                    if !(v.is_finite() && v >= 0.0) {
                        reject(state, "config.opacity", &format!("{name} {v}"));
                        return;
                    }
                }
            }
            let current = state
                .config
                .opacity
                .get_or_insert_with(viewport_ipc::event::Opacity::default);
            let mut changed = false;
            if let Some(v) = active {
                current.active = v;
                changed = true;
            }
            if let Some(v) = inactive {
                current.inactive = v;
                changed = true;
            }
            if let Some(v) = fullscreen {
                current.fullscreen = v;
                changed = true;
            }
            if changed {
                // Multiplied by the shell's per-window opacity at render
                // time, so the new policy is not painted until a frame.
                state.needs_render = true;
                state.notify_config();
            }
        }

        Request::ConfigIdle {
            lock_after,
            blank_after,
            lock_delay,
            lock_command,
        } => {
            // Validated as a whole before anything changes, like ConfigGaps:
            // a negative deadline takes the whole message with it.
            for (name, value) in [
                ("lock_after", lock_after),
                ("blank_after", blank_after),
                ("lock_delay", lock_delay),
            ] {
                if let Some(v) = value {
                    if v < 0 {
                        reject(state, "config.idle", &format!("{name} {v}"));
                        return;
                    }
                }
            }
            let changed = lock_after.is_some()
                || blank_after.is_some()
                || lock_delay.is_some()
                || lock_command.is_some();
            if let Some(v) = lock_after {
                state.idle_settings.lock_after = Some(v);
            }
            if let Some(v) = blank_after {
                state.idle_settings.blank_after = Some(v);
            }
            if let Some(v) = lock_delay {
                state.idle_settings.lock_delay = Some(v);
            }
            if let Some(v) = lock_command.as_ref() {
                state.idle_settings.lock_command = Some(v.clone());
            }
            if changed {
                // The lock mode, the re-armed timers and the inhibit state
                // all move with the settings; the Config event carries them
                // so the panel shows the switch where it really is and
                // `config.save` writes what is on screen. A lock command
                // that blinks the mode must not wake a blanked desk either
                // way, which is `Idle::rearm`'s whole point.
                state.apply_idle_settings();
                let current = state
                    .config
                    .idle
                    .get_or_insert_with(viewport_ipc::event::Idle::default);
                if let Some(v) = lock_after {
                    current.lock_after = Some(v);
                }
                if let Some(v) = blank_after {
                    current.blank_after = Some(v);
                }
                if let Some(v) = lock_delay {
                    current.lock_delay = Some(v);
                }
                if let Some(v) = lock_command.as_ref() {
                    current.lock_command = Some(v.clone());
                }
                state.notify_config();
            }
        }

        Request::ConfigLid { action } => {
            if let Some(action) = action {
                match crate::power::LidAction::parse(&action) {
                    Some(parsed) => {
                        state.apply_lid(parsed);
                        state.config.lid = Some(action);
                        state.notify_config();
                    }
                    None => {
                        reject(state, "config.lid", &format!("unknown action {action:?}"));
                    }
                }
            }
        }

        Request::ConfigKeyboard {
            layout,
            variant,
            options,
            rules,
            model,
            repeat_rate,
            repeat_delay,
        } => {
            // Validated as a whole before anything changes, like ConfigGaps:
            // a repeat rate or delay of zero or below takes the whole
            // message with it, and a keymap xkb refuses is rejected rather
            // than recorded — `apply_keyboard` only writes down what it
            // applied, so the Config copy below can only be reached on
            // success.
            for (name, value) in [("repeat_rate", repeat_rate), ("repeat_delay", repeat_delay)] {
                if let Some(v) = value {
                    if v <= 0 {
                        reject(state, "config.keyboard", &format!("{name} {v}"));
                        return;
                    }
                }
            }
            let mut keyboard = state.keyboard_config.clone();
            if let Some(v) = layout {
                keyboard.layout = Some(v);
            }
            if let Some(v) = variant {
                keyboard.variant = Some(v);
            }
            if let Some(v) = options {
                keyboard.options = Some(v);
            }
            if let Some(v) = rules {
                keyboard.rules = Some(v);
            }
            if let Some(v) = model {
                keyboard.model = Some(v);
            }
            if let Some(v) = repeat_rate {
                keyboard.repeat_rate = Some(v);
            }
            if let Some(v) = repeat_delay {
                keyboard.repeat_delay = Some(v);
            }
            match state.apply_keyboard(&keyboard) {
                Ok(_) => {
                    // The keymap was rebuilt, or was already what the desk
                    // types in: either way the Config copy is the candidate
                    // and the panel shows what it is.
                    state.config.keyboard = Some(viewport_ipc::event::Keyboard {
                        layout: keyboard.layout.clone(),
                        variant: keyboard.variant.clone(),
                        options: keyboard.options.clone(),
                        rules: keyboard.rules.clone(),
                        model: keyboard.model.clone(),
                        repeat_rate: keyboard.repeat_rate,
                        repeat_delay: keyboard.repeat_delay,
                    });
                    state.notify_config();
                }
                Err(e) => {
                    reject(state, "config.keyboard", &e);
                }
            }
        }

        Request::ConfigInput {
            device,
            tap,
            tap_drag,
            natural_scroll,
            left_handed,
            middle_emulation,
            disable_while_typing,
            accel_speed,
            accel_profile,
            click_method,
            scroll_method,
            scroll_factor,
            emulate_discrete_scroll,
        } => {
            // Validated as a whole before anything changes, like ConfigGaps:
            // an unnamed device, an acceleration outside libinput's range, a
            // profile or method nobody knows, or a zero scroll factor takes
            // the whole message with it.
            let Some(device) = device.filter(|name| !name.trim().is_empty()) else {
                reject(state, "config.input", "no device named");
                return;
            };
            if let Some(v) = accel_speed {
                if !(v.is_finite() && (-1.0..=1.0).contains(&v)) {
                    reject(state, "config.input", &format!("accel_speed {v}"));
                    return;
                }
            }
            if let Some(v) = accel_profile.as_deref() {
                if !["adaptive", "flat"].contains(&v) {
                    reject(state, "config.input", &format!("accel_profile {v:?}"));
                    return;
                }
            }
            if let Some(v) = click_method.as_deref() {
                if !["button_areas", "clickfinger"].contains(&v) {
                    reject(state, "config.input", &format!("click_method {v:?}"));
                    return;
                }
            }
            if let Some(v) = scroll_method.as_deref() {
                if !["none", "two_finger", "edge", "on_button_down"].contains(&v) {
                    reject(state, "config.input", &format!("scroll_method {v:?}"));
                    return;
                }
            }
            if let Some(v) = scroll_factor {
                if !(v.is_finite() && v != 0.0) {
                    reject(state, "config.input", &format!("scroll_factor {v}"));
                    return;
                }
            }
            let entry = state.input_config.entry(device.clone()).or_default();
            if let Some(v) = tap {
                entry.tap = Some(v);
            }
            if let Some(v) = tap_drag {
                entry.tap_drag = Some(v);
            }
            if let Some(v) = natural_scroll {
                entry.natural_scroll = Some(v);
            }
            if let Some(v) = left_handed {
                entry.left_handed = Some(v);
            }
            if let Some(v) = middle_emulation {
                entry.middle_emulation = Some(v);
            }
            if let Some(v) = disable_while_typing {
                entry.disable_while_typing = Some(v);
            }
            if let Some(v) = accel_speed {
                entry.accel_speed = Some(v);
            }
            if let Some(v) = accel_profile {
                entry.accel_profile = Some(v);
            }
            if let Some(v) = click_method {
                entry.click_method = Some(v);
            }
            if let Some(v) = scroll_method {
                entry.scroll_method = Some(v);
            }
            if let Some(v) = scroll_factor {
                entry.scroll_factor = Some(v);
            }
            if let Some(v) = emulate_discrete_scroll {
                entry.emulate_discrete_scroll = Some(v);
            }
            // The devices already connected are reconfigured at once, as a
            // reload does; a device that arrives later reads the same map.
            state.apply_libinput_config();
            state.config.input = Some(crate::state::input_event_map(&state.input_config));
            state.notify_config();
        }

        Request::ConfigNotifications {
            sound_file,
            sound_name,
            history,
        } => {
            // Nothing to validate beyond the type: a sound that resolves to
            // nothing is silence, which is what absence already means.
            let current = state
                .config
                .notifications
                .get_or_insert_with(viewport_ipc::event::Notifications::default);
            let mut changed = false;
            if let Some(v) = sound_file {
                current.sound_file = Some(v);
                changed = true;
            }
            if let Some(v) = sound_name {
                current.sound_name = Some(v);
                changed = true;
            }
            if let Some(v) = history {
                current.history = Some(v);
                changed = true;
            }
            if changed {
                // The sound player and the history are re-read off the
                // merged copy, so lowering the history drops the oldest
                // entries there and then, as a reload does.
                let current = state.config.notifications.clone().unwrap_or_default();
                state.apply_notifications_config(
                    current.sound_file.as_deref(),
                    current.sound_name.as_deref(),
                    current.history,
                );
                state.notify_config();
            }
        }

        Request::ConfigMagnify { step, max } => {
            // Validated as a whole before anything changes, like ConfigGaps:
            // a step or ceiling that is not a finite positive number takes
            // the whole message with it. The effective values — clamped to
            // the smallest worth having and the zoom ceiling — are what the
            // Config copy carries, read back off the magnifier rather than
            // remembered as they were asked for.
            for (name, value) in [("step", step), ("max", max)] {
                if let Some(v) = value {
                    if !(v.is_finite() && v > 0.0) {
                        reject(state, "config.magnify", &format!("{name} {v}"));
                        return;
                    }
                }
            }
            if step.is_some() || max.is_some() {
                let step = step.unwrap_or_else(|| state.magnifier.step());
                let max = max.unwrap_or_else(|| state.magnifier.max());
                if state.magnifier.configure(Some(step), Some(max)) {
                    state.needs_render = true;
                }
                state.config.magnify = Some(viewport_ipc::event::Magnify {
                    step: state.magnifier.step(),
                    max: state.magnifier.max(),
                });
                state.notify_config();
            }
        }

        Request::ConfigSave => config_save(state),

        Request::Quit => state.shutdown(),
    }
}

/// Configure an X11 window to cover its output, at the moment fullscreen is
/// granted to it.
///
/// `view_layout` will not configure a fullscreen X11 window at all — the
/// rectangles it sees while anything is animating are ones wine reads as its
/// handshake having been refused, and drops back out of fullscreen on
/// (`apply: stop configuring a fullscreen X11 window from the layout`). That
/// guard is about the journey; the destination still has to be sent once, and
/// the note over the skip assumed the request path already had — which was
/// only ever true for a client that sized its own window before mapping (a
/// wine game, `mpv --fullscreen`). A window the SHELL fullscreened — the
/// `window.fullscreen` chord, `view.fullscreen` over the socket — was granted
/// the state, lost its frame, and stayed exactly the size it was: a
/// borderless tile instead of a covered output.
///
/// EWMH's answer for every client, whichever way it got here: "The window
/// MUST be resized to the size of the monitor." One configure per grant, at
/// the output the window is on, deduplicated against `last_configure` so a
/// client already holding this rectangle — a game that mapped at the output's
/// size — is told nothing at all. Wine counts configures.
pub(crate) fn configure_x11_fullscreen(state: &mut ViewportState, id: u32) {
    let Some(view) = state.views.get(id) else {
        return;
    };
    let Some(x11) = view.window.x11_surface().cloned() else {
        // The xdg side needs nothing here: `view_layout` configures a
        // toplevel's size whether it is fullscreen or not.
        return;
    };
    let min = view.min_size();
    // The output the window is on, and fullscreen covers that one. Before the
    // first layout there is no element in the space to ask about, and the
    // answer then is the one the announce path already gave the shell: the
    // output a new window is told it is on.
    let output = state
        .space
        .outputs_for_element(&view.window)
        .into_iter()
        .next()
        .or_else(|| state.output_by_name(&state.output_for_new_view()));
    let Some(geometry) = output.and_then(|output| state.space.output_geometry(&output)) else {
        return;
    };
    // Clamped exactly as `view_layout` clamps what the shell asks for: a
    // client configured under its minimum is entitled to ignore it, and a
    // window that ignores this one is the bug again.
    let (width, height) = configure_size((geometry.size.w, geometry.size.h), min);
    let rect = smithay::utils::Rectangle::new(geometry.loc, (width, height).into());
    if x11.last_configure() == rect {
        return;
    }
    if let Err(e) = x11.configure(rect) {
        tracing::warn!("could not configure an X11 window into fullscreen: {e}");
    }
}

/// Apply fullscreen state to either kind of managed client.
///
/// Resizing the hole is not enough: applications rearrange their own layout
/// when this state changes. Shared by shell IPC and foreign-toplevel requests
/// so neither path can update only the frame around the client.
pub(crate) fn set_view_fullscreen(state: &mut ViewportState, id: u32, fullscreen: bool) -> bool {
    let Some(window) = state.views.get(id).map(|view| view.window.clone()) else {
        return false;
    };
    if let Some(toplevel) = window.toplevel() {
        toplevel.with_pending_state(|pending| {
            if fullscreen {
                pending.states.set(xdg_toplevel::State::Fullscreen);
            } else {
                pending.states.unset(xdg_toplevel::State::Fullscreen);
            }
        });
        toplevel.send_pending_configure();
    } else if let Some(x11) = window.x11_surface() {
        if let Err(e) = x11.set_fullscreen(fullscreen) {
            tracing::warn!("could not set fullscreen on an X11 window: {e}");
            return false;
        }
        // The state alone is not the resize. See `configure_x11_fullscreen`:
        // the layout will not configure this window again while it is
        // fullscreen, so the covering rectangle has to come from here.
        if fullscreen {
            configure_x11_fullscreen(state, id);
        }
    } else {
        return false;
    }

    state.foreign_management_state.set_state(
        id,
        state.focused == id,
        state.view_is_maximized(id),
        state.view_is_minimized(id),
        fullscreen,
    );
    state.needs_render = true;
    true
}

/// Apply maximized state to either kind of managed client.
pub(crate) fn set_view_maximized(state: &mut ViewportState, id: u32, maximized: bool) -> bool {
    let Some(window) = state.views.get(id).map(|view| view.window.clone()) else {
        return false;
    };
    if let Some(toplevel) = window.toplevel() {
        toplevel.with_pending_state(|pending| {
            if maximized {
                pending.states.set(xdg_toplevel::State::Maximized);
            } else {
                pending.states.unset(xdg_toplevel::State::Maximized);
            }
        });
        toplevel.send_pending_configure();
    } else if let Some(x11) = window.x11_surface() {
        if let Err(e) = x11.set_maximized(maximized) {
            tracing::warn!("could not set maximized on an X11 window: {e}");
            return false;
        }
    } else {
        return false;
    }

    state.foreign_management_state.set_state(
        id,
        state.focused == id,
        maximized,
        state.view_is_minimized(id),
        state.view_is_fullscreen(id),
    );
    true
}

/// Set compositor-owned minimize state and mirror it to Xwayland and observers.
pub(crate) fn set_view_minimized(state: &mut ViewportState, id: u32, minimized: bool) -> bool {
    let Some(window) = state.views.get(id).map(|view| view.window.clone()) else {
        return false;
    };

    if let Some(x11) = window.x11_surface() {
        if let Err(e) = x11.set_hidden(minimized) {
            tracing::warn!("could not set hidden state on an X11 window: {e}");
            return false;
        }
    }
    if let Some(view) = state.views.get_mut(id) {
        view.minimized = minimized;
    }

    state.foreign_management_state.set_state(
        id,
        state.focused == id && !minimized,
        state.view_is_maximized(id),
        minimized,
        state.view_is_fullscreen(id),
    );

    if minimized && state.focused == id {
        if let Some(keyboard) = state.seat.get_keyboard() {
            let serial = SERIAL_COUNTER.next_serial();
            keyboard.set_focus(
                state,
                Option::<crate::keyboard_focus::KeyboardFocus>::None,
                serial,
            );
        }
        state.notify_focus(NO_VIEW);
    }
    true
}

/// Whether a head is inside the pending output-revert window.
///
/// A mode, scale, rotation or power change arms a twelve-second countdown
/// (`ViewportState::arm_output_revert`). During it the configuration on screen
/// is the one nobody has confirmed, and the compositor is going to put the head
/// back. Writing that head into the settings overlay now would persist the
/// mode or scale the person could not read the screen to reject — and because
/// the overlay is rewritten whole, a `config.save` from the shell or the CLI
/// during the countdown is the one way that still happens now that the panel
/// disables its Save button while confirming.
fn head_is_unconfirmed(
    revert: Option<&[crate::output_management::HeadChange]>,
    name: &str,
) -> bool {
    revert.is_some_and(|changes| changes.iter().any(|change| change.name == name))
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
        // Some("") rather than None: absent means "leave what the file
        // said", so a picture the panel cleared would come back on the next
        // start. The empty string is the config file's "no wallpaper".
        wallpaper: Some(state.config.wallpaper.clone().unwrap_or_default()),
        wallpaper_mode: state.config.wallpaper_mode.clone(),
        gaps: state.config.gaps.clone(),
        border: state.config.border.clone(),
        cursor: state.config.cursor.clone(),
        motion: state.config.motion.clone(),
        group: state.config.group.clone(),
        clock: state.config.clock.clone(),
        bar: state.config.bar.clone(),
        bar_blur: Some(state.config.bar_blur),
        bar_widgets: state.config.bar_widgets.clone(),
        bar_items: state.config.bar_items.clone(),
        logo: Some(state.config.logo),
        tutorial: Some(state.config.tutorial),
        tiling_mode: state.config.tiling_mode.clone(),
        layout: Some(state.config.layout.clone()),
        layout_extensions: if state.config.layout_extensions.is_empty() {
            None
        } else {
            Some(
                state
                    .config
                    .layout_extensions
                    .iter()
                    .map(|entry| (entry.name.clone(), entry.url.clone()))
                    .collect(),
            )
        },
        focus_crosses_outputs: Some(state.config.focus_crosses_outputs),
        osk: Some(state.config.osk.clone()),
        tray: state.config.tray,
        clipboard_history: state.config.clipboard_history,
        icon_theme: state.config.icon_theme.clone(),
        theme: state.config.theme.clone(),
        rules: state.config.rules.clone(),
        layer_rules: state.config.layer_rules.clone(),
        decorations: state.config.decorations.clone(),
        workspaces: if state.config.workspaces.is_empty() {
            None
        } else {
            // The shell's list back in the file's map form, so the overlay
            // parses as the file's own keys.
            let workspaces: std::collections::HashMap<u32, crate::config::WorkspaceConfig> = state
                .config
                .workspaces
                .iter()
                .map(|rule| {
                    (
                        rule.workspace,
                        crate::config::WorkspaceConfig {
                            output: rule.output.clone(),
                            layout: rule.layout.clone(),
                            tiling_mode: rule.tiling_mode.clone(),
                            gaps: crate::config::GapsConfig {
                                inner: rule.gaps.as_ref().and_then(|gaps| gaps.inner),
                                outer: rule.gaps.as_ref().and_then(|gaps| gaps.outer),
                                smart: rule.gaps.as_ref().and_then(|gaps| gaps.smart),
                            },
                            persistent: rule.persistent,
                            default_name: rule.default_name.clone(),
                            on_created_empty: rule.on_created_empty.clone(),
                            no_border: rule.no_border,
                            no_rounding: rule.no_rounding,
                            decorate: rule.decorate,
                            float_gaps: rule.float_gaps,
                            animation: rule.animation.clone(),
                            layout_opts: rule.layout_opts.clone(),
                        },
                    )
                })
                .collect();
            serde_json::to_value(&workspaces).ok()
        },
        opacity: state.config.opacity.clone(),
        idle: state.config.idle.clone(),
        lid: state.config.lid.clone(),
        keyboard: state.config.keyboard.clone(),
        input: state.config.input.clone(),
        notifications: state.config.notifications.clone(),
        magnify: state.config.magnify.clone(),
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
        if head_is_unconfirmed(state.output_revert.as_deref(), &name) {
            // Its current mode and scale belong to a configuration that is
            // about to be reverted; saving them would outlive the countdown
            // and the confirmation it was waiting for.
            tracing::info!("{name}: not saving an unconfirmed output configuration");
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
                // Some("") rather than None: absent means "leave what the
                // file said", so a mirror the panel detached would come back
                // on the next start. The empty string is how the request and
                // the config file spell "not mirrored".
                mirror: Some(
                    state
                        .output_mirrors
                        .get(&output.name())
                        .cloned()
                        .unwrap_or_default(),
                ),
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

/// Keep the clip the shell meant, drop one that cannot be one.
///
/// A window is cropped to the hole the shell drew for it so that a window
/// scrolled half off its column, or mid-animation, cannot paint over its
/// neighbours — and a column scrolled *entirely* out of its strip is cropped to
/// nothing on purpose. That empty crop is how the scrolling layout hides the
/// columns on either side of the focused one, so it is honoured: drawing such a
/// window "uncropped to be safe" is how a column from the right monitor ends up
/// painted onto the left one.
///
/// What cannot be an intended crop is one with area that does not meet the
/// window at all: the shell computes the clip as an intersection with the area
/// reserved for windows, and an intersection that misses its own rectangle
/// means the rectangle and the area disagreed — the window would be cropped
/// away while the shell's frame stayed drawn, which is the see-through hole
/// with a border around it. That one is dropped, once per window, so the
/// window is visible while the disagreement is still worth a warning.
fn usable_clip(
    id: u32,
    clip: Option<viewport_ipc::geometry::Box>,
    box_: viewport_ipc::geometry::Box,
    warn: bool,
) -> Option<viewport_ipc::geometry::Box> {
    let clip = clip?;
    // Empty in either direction: the shell means "none of this window". Honoured
    // exactly as sent, which is the whole point of a crop.
    if clip.width <= 0 || clip.height <= 0 {
        return Some(clip);
    }
    let right = clip.x.saturating_add(clip.width);
    let bottom = clip.y.saturating_add(clip.height);
    let box_right = box_.x.saturating_add(box_.width);
    let box_bottom = box_.y.saturating_add(box_.height);
    let meets = clip.x < box_right && clip.y < box_bottom && right > box_.x && bottom > box_.y;
    if meets {
        return Some(clip);
    }
    if warn {
        tracing::warn!(
            "view {id}: the shell clipped it to a rectangle that does not meet it \
             ({}x{} at {:+}{:+} against {}x{} at {:+}{:+}); drawing it uncropped",
            clip.width,
            clip.height,
            clip.x,
            clip.y,
            box_.width,
            box_.height,
            box_.x,
            box_.y
        );
    }
    None
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
    let Some(mut resolved) = layout.resolve(view.box_) else {
        // A degenerate box is dropped without an error, as in the C build.
        return;
    };
    // A clip with no area in it cuts every surface the window has away. Its
    // frame is the shell's and stays drawn, so what is left is exactly the
    // reported see-through window: a frame with the desktop showing through it.
    // It is never a crop worth honouring — the shell computes the clip from the
    // hole it drew against the area reserved for windows, so an empty
    // intersection means that area and the rectangle disagree, and cropping to
    // it is the one answer that cannot be right.
    let asked_clip = resolved.clip;
    let warn = !state
        .views
        .get(layout.id)
        .is_some_and(|view| view.clip_mismatch_warned);
    resolved.clip = usable_clip(layout.id, asked_clip, resolved.box_, warn);
    // Remembered so a shell that keeps sending the same disagreement does not
    // fill the log while the window is being dragged; a later layout that
    // agrees again clears it, so a recurrence is still said out loud.
    let mismatch = asked_clip.is_some() && resolved.clip.is_none();

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
    // Everything the renderer draws from this window, before this layout is
    // applied. The question the end of this function answers is whether any
    // of it is about to change; see the note there.
    let was = (
        view.box_,
        view.scale,
        view.clip,
        view.frame,
        view.floating,
        view.square,
        view.visible,
        view.placed,
    );
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
    view.clip_mismatch_warned = mismatch;
    // A rectangle un-hides a window, as in C (`src/xdg_shell.c:832`).
    //
    // The shell hides a window by sending `view.visible false` — that is what
    // a workspace switch is — and shows it again by laying it out, without
    // sending `visible` a second time. Treating those as independent left a
    // window hidden for the rest of the session: the shell kept drawing its
    // frame, because as far as the shell is concerned it is on screen, and
    // the compositor drew nothing inside it.
    view.visible = true;
    // The rectangle the client is about to be placed at, position included.
    // Two gates come out of the record: `resize`, on the size half alone,
    // because a toplevel is moved without a configure and must not pay for
    // one; and `relaid`, on the whole rectangle, for X — which is told where
    // it is and would otherwise draw its menus at the old place. The record
    // updates either way; it is what the next layout is compared against.
    let rect = Configured(width, height, resolved.box_.x, resolved.box_.y);
    let resize = view.configured.map(Configured::size) != Some((width, height));
    let relaid = view.configured != Some(rect);
    view.configured = Some(rect);
    // Whether anything the renderer draws from just moved, spent at the end
    // where the rest of what this layout owes is recorded.
    let redraw = was
        != (
            view.box_,
            view.scale,
            view.clip,
            view.frame,
            view.floating,
            view.square,
            view.visible,
            view.placed,
        );

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
    //
    // Unless it is fullscreen. Then the only rectangle it will accept is the
    // one it negotiated, and every frame of a shell animation in between is a
    // rectangle the client reads as its request having been refused: wine
    // leaves fullscreen again on the first configure that is not its own, and
    // a window that asked to cover its output has no journey left to make
    // anyway. The destination rectangle is sent once, by the grant path (see
    // `configure_x11_fullscreen`) and by the settled-rectangle branch below,
    // and the state is what the shell's rectangle falls back to when it stops
    // being fullscreen.
    //
    // And not when the client already holds this exact rectangle. The shell
    // lays every window out on every frame of an animation and Smithay's
    // `configure` has no identical-rectangle dedup — it sends a
    // ConfigureNotify and flushes every time — so every frame of *another*
    // window's animation used to be an X round trip and a client repaint for
    // every X window on the desk.
    //
    // Two records guard the skip, because either alone can be wrong here.
    // `relaid` — this rectangle against the previous layout's — cannot see
    // the configures other paths make: the map answer, a configure-request
    // answer, a fullscreen negotiation. `last_configure` is what Smithay
    // last actually sent, from wherever, and a configure that failed leaves
    // it untouched so the next layout tries again rather than believing the
    // client was told. Only when both say the client has it is the send
    // skipped; the toplevel above is spared by its size half and needs no
    // position at all.
    if let Some(x11) = window.x11_surface() {
        let x11_rect = smithay::utils::Rectangle::new(
            (resolved.box_.x, resolved.box_.y).into(),
            (width, height).into(),
        );
        if !x11.is_fullscreen() {
            if relaid || x11.last_configure() != x11_rect {
                if let Err(e) = x11.configure(x11_rect) {
                    tracing::warn!("could not configure an X11 window: {e}");
                }
            }
        } else {
            // The one rectangle a fullscreen window may be configured with is
            // its output's — which is what the settled layout of a fullscreen
            // window is, and the only one that is safe to send while the state
            // stands. Without it a window that crossed onto another output
            // while fullscreen keeps covering the rectangle it started on.
            // Everything else here is a rectangle still in flight from some
            // animation, which is exactly the configure wine drops fullscreen
            // on and the reason this branch does not simply fall through.
            //
            // Deduplicated on `last_configure` alone rather than `relaid`:
            // the settled rectangle arrives more than once — the grant path
            // (see `configure_x11_fullscreen`) usually sent it first, and
            // transient layouts are skipped in between — and wine counts
            // configures.
            let settled = {
                let resolved_rect = smithay::utils::Rectangle::new(
                    (resolved.box_.x, resolved.box_.y).into(),
                    (resolved.box_.width, resolved.box_.height).into(),
                );
                state
                    .space
                    .outputs()
                    .any(|output| state.space.output_geometry(output) == Some(resolved_rect))
            };
            if settled && x11.last_configure() != x11_rect {
                if let Err(e) = x11.configure(x11_rect) {
                    tracing::warn!("could not configure an X11 window: {e}");
                }
            }
        }
    }

    state
        .space
        .map_element(window, (resolved.box_.x, resolved.box_.y), false);

    // The frame that will carry this rectangle has not been drawn yet, so a
    // `drawn_at` from before it says nothing about it. Recording the debt here
    // is what lets `settle_undrawn_views` catch the case where no frame ever
    // pays it out — the new window that stays missing until something else
    // forces a redraw. See the note on that function.
    state.undrawn_pending = true;
    if let Some(view) = state.views.get_mut(id) {
        view.placed_at = Some(std::time::Instant::now());
        view.redraw_asks = 0;
    }

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
    // a different monitor's list now. Owed like the rest, but per view:
    // `settle` asks about only the views a run of `view.layout` messages named,
    // and `set_outputs` diffs each against what it last sent, so an unchanged
    // window sends nothing. See `foreign_outputs_dirty`.
    if !state.foreign_outputs_dirty.contains(&layout.id) {
        state.foreign_outputs_dirty.push(layout.id);
    }

    // And pixels, where they changed. A window moved, re-clipped, re-framed
    // or shown back up is drawn somewhere it was not, and no commit follows
    // to mark that — the client did nothing. `render_if_needed` draws for
    // `needs_render` and nothing else, so a script's one `view.layout` on a
    // still desk, or the watchdog's rescue columns with the shell dead, left
    // the change on the floor until something unrelated drew. A layout
    // that changed none of it — the shell resending the same rectangle on
    // every frame of some other window's animation — needs no redraw and
    // gets no mark.
    if redraw {
        state.needs_render = true;
    }
}

pub fn focus_view(state: &mut ViewportState, id: u32) {
    // A locked session has one destination for the keyboard, and this is not
    // it. The IPC path and the foreign-toplevel request already refuse while
    // locked; this is the floor under every caller, including the portal
    // picker whose focus restore runs as the chooser comes down.
    if state.locked {
        tracing::debug!("ignoring focus for view {id} while the session is locked");
        return;
    }
    // Activation is also restore. This covers shell taskbars, foreign
    // toplevels, and direct IPC with one rule rather than leaving a focused
    // surface omitted from layout.
    if state.view_is_minimized(id) {
        if !set_view_minimized(state, id, false) {
            return;
        }
        state.notify_minimized(id, false);
    }
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

/// The modeline a `mode` request resolves to, if it names one.
///
/// An exact modeline the display advertised wins; a custom one is the fallback
/// for unusual panels. A request with no refresh (`0`) takes the first
/// advertised mode at that size, which is not necessarily the one being driven
/// now — so this is also what tells "any rate at this size" from "already
/// there" for the no-op checks below.
fn resolve_output_mode(output: &Output, requested: ModeRequest) -> Option<OutputMode> {
    let exact = output.modes().into_iter().find(|m| {
        m.size.w == requested.width
            && m.size.h == requested.height
            && (requested.refresh == 0 || m.refresh == requested.refresh)
    });
    exact.or_else(|| {
        (requested.width > 0 && requested.height > 0 && requested.refresh > 0).then(|| OutputMode {
            size: (requested.width, requested.height).into(),
            refresh: requested.refresh,
        })
    })
}

/// Whether `config` would leave `output` exactly as it already is.
///
/// Only the fields the request names are compared: an absent field is an
/// instruction to leave that one alone, not a request to reset it. A request
/// to switch off a screen that is already off is the whole request, because
/// `output_configure` returns from its disabled branch before reading any other
/// field — that first branch still owes the layout tell, which is why the
/// caller asks about the first application separately.
pub(crate) fn output_configure_matches_live(
    state: &ViewportState,
    output: &Output,
    config: &OutputConfigure,
) -> bool {
    if config.enabled == Some(false) {
        return !state.output_is_enabled(output);
    }

    let resolved_mode = config
        .mode
        .and_then(|requested| resolve_output_mode(output, requested));
    if let Some(mode) = resolved_mode {
        // Preferred as well as current: naming a mode also makes it the
        // advertised preferred one, so a head whose current mode matches but
        // whose preferred mode does not still has that repair coming.
        if output.current_mode() != Some(mode) || output.preferred_mode() != Some(mode) {
            return false;
        }
    }
    if let Some(scale) = config.scale {
        if output.current_scale().fractional_scale() != scale {
            return false;
        }
    }
    if let Some(transform) = config.transform {
        if output.current_transform() != to_smithay_transform(transform) {
            return false;
        }
    }
    // The map `output_configure` writes, not `configured_vrr`: a request that
    // pins the policy this head already has is a change even when the global
    // `adaptive_sync` default happens to give the same effective answer.
    let wanted_vrr = config.vrr.or(config.adaptive_sync.map(|enabled| {
        if enabled {
            viewport_ipc::event::VrrMode::Always
        } else {
            viewport_ipc::event::VrrMode::Off
        }
    }));
    if let Some(vrr) = wanted_vrr {
        if state.output_vrr.get(&output.name()) != Some(&vrr) {
            return false;
        }
    }
    if config.x.is_some() || config.y.is_some() {
        let Some(geometry) = state.space.output_geometry(output) else {
            // The path below maps the output at the requested position, so an
            // unmapped one is work even when the numbers agree with whatever
            // it carries while dark.
            return false;
        };
        if config.x.is_some_and(|x| x != geometry.loc.x)
            || config.y.is_some_and(|y| y != geometry.loc.y)
        {
            return false;
        }
    }

    // A mode, a scale or a rotation maps an output that is switched off back
    // into the space as it applies, so one of those named for an unmapped
    // output is work even when its value matches what the output carries.
    let geometry_named =
        resolved_mode.is_some() || config.scale.is_some() || config.transform.is_some();
    if !state.output_is_enabled(output) && (geometry_named || config.enabled == Some(true)) {
        return false;
    }
    true
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
        // `NaN <= 0.0` is false, so an infinite or NaN scale used to pass
        // this and reach the renderer. Every use of it assumes a real,
        // positive number. And a finite one is not enough on its own: `1e300`
        // collapses the geometry to a pixel and later overflows the cursor
        // load, so the same bounded predicate as the output-management path
        // is the one that decides.
        if !crate::output_management::output_scale_ok(scale) {
            reject(
                state,
                "output.configure",
                &format!("scale {scale} is outside the accepted range"),
            );
            return;
        }
    }
    // The same bounds the wlr-output-management path enforces. This request
    // comes from the shell or the CLI, so it is a trusted sender — but a page
    // mistake must not become a compositor abort, and an unbounded custom mode
    // here bypasses every check `apply_output_configuration` makes.
    if let Some(mode) = config.mode {
        use crate::output_management::custom_mode_size_ok;
        if !custom_mode_size_ok(mode.width, mode.height) {
            reject(
                state,
                "output.configure",
                "mode is larger than the compositor will carry",
            );
            return;
        }
    }
    {
        use crate::output_management::layout_coord_ok;
        for coord in [config.x, config.y].into_iter().flatten() {
            if !layout_coord_ok(coord) {
                reject(
                    state,
                    "output.configure",
                    "position is outside the accepted layout",
                );
                return;
            }
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
    // Whether the mapping this request names differs from the one in place.
    // `mirror` is the one field whose change is applied before the no-op check
    // below; `configure_mirror` skips the teardown and rebuild when it does
    // not change, but the tail after a change still has to run.
    let previous_mirror = state.output_mirrors.get(&config.name).cloned();
    let wanted_mirror = config
        .mirror
        .as_deref()
        .filter(|source| !source.is_empty())
        .map(str::to_owned);
    let mirror_changed = config.mirror.is_some() && previous_mirror != wanted_mirror;
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

    // A request that names nothing different from what the output is already
    // doing is not a reason to send `wl_output.done`, re-arrange the layers,
    // reset the DRM buffers if the shape moved, or tell every client the layout
    // changed. The first request for an output is never treated as a no-op
    // whatever it says: it is what introduces the head to the shell and
    // output-management clients, which a head that came up exactly as the file
    // asks still needs.
    let first_application = !state.output_configure_applied.contains(&config.name);
    if !mirror_changed
        && !first_application
        && output_configure_matches_live(state, &output, &config)
    {
        return;
    }

    // Everything above this point refuses or returns without doing anything;
    // everything below it changes the hardware. So this is where the change
    // becomes provisional and where the monitor is written down as one
    // somebody has an opinion about — after the last `return` that means
    // "nothing happened", and before the first line that means something did.
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
            state.output_configure_applied.insert(config.name.clone());
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
    let mode = config
        .mode
        .and_then(|requested| resolve_output_mode(&output, requested));

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

    // The description fields first: a transform or a scale is part of the
    // shape the mode is arranged into, so it has to be in place before
    // `set_output_mode` re-arranges the layer map. The mode itself is not
    // passed here — `set_output_mode` is the one routine that programs the
    // CRTC, and `change_current_state` alone moves what every client is told
    // while a real panel keeps scanning out the old resolution.
    if transform.is_some() || scale.is_some() {
        output.change_current_state(None, transform, scale, None);
    }
    if let Some(mode) = mode {
        state.set_output_mode(&output, mode);
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
    if mode.is_none() && (transform.is_some() || scale.is_some()) {
        // `set_output_mode` has already re-arranged an output whose mode
        // changed, with the transform and scale above in place. This is what
        // arranges a transform or scale on its own.
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
    state.output_configure_applied.insert(config.name.clone());
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
/// not replacing the other. `release` is part of it for the same reason:
/// `release+Mod4+q` is the other half of the key, not another name for the
/// press, and matching on everything but that made registering one replace
/// the other.
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
        && a.locked == b.locked
        && a.release == b.release
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
    fn an_empty_crop_is_a_crop_and_a_misplaced_one_is_not() {
        let window = viewport_ipc::geometry::Box::new(10, 20, 300, 200);
        let full = viewport_ipc::geometry::Box::new(0, 0, 1920, 1080);
        assert_eq!(super::usable_clip(1, Some(full), window, true), Some(full));
        // Empty in either direction is how the scrolling strip hides a column
        // scrolled out of view. Honoured, not "fixed".
        let empty_width = viewport_ipc::geometry::Box::new(10, 20, 0, 200);
        let empty_height = viewport_ipc::geometry::Box::new(10, 20, 300, 0);
        assert_eq!(
            super::usable_clip(1, Some(empty_width), window, true),
            Some(empty_width)
        );
        assert_eq!(
            super::usable_clip(1, Some(empty_height), window, true),
            Some(empty_height)
        );
        // Has area and lands somewhere else entirely: that cannot be the
        // shell's own intersection, so it is dropped and the window drawn.
        assert_eq!(
            super::usable_clip(
                1,
                Some(viewport_ipc::geometry::Box::new(-500, -500, 300, 200)),
                window,
                true
            ),
            None
        );
        // Touching only at an edge cannot carry any of the window either.
        assert_eq!(
            super::usable_clip(
                1,
                Some(viewport_ipc::geometry::Box::new(310, 20, 300, 200)),
                window,
                true
            ),
            None
        );
        // Half over the edge of its own column is the case the crop exists
        // for, and it is kept.
        let half = viewport_ipc::geometry::Box::new(-150, 0, 300, 200);
        assert_eq!(super::usable_clip(1, Some(half), window, true), Some(half));
        assert_eq!(super::usable_clip(1, None, window, true), None);
    }

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
            crate::binding::match_binding(&bindings, &free, keysyms::KEY_h, "resize", false, false)
                .is_some(),
            "the resize-mode binding for h is gone"
        );
        assert!(
            crate::binding::match_binding(&bindings, &free, keysyms::KEY_h, "", false, false)
                .is_some()
        );

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
            crate::binding::match_button(&bindings, &held, 0x113, "", false).is_some(),
            "the Mouse4 binding is gone"
        );
        assert_eq!(
            crate::binding::match_wheel(&bindings, &held, Wheel::Down, "", false),
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
            crate::binding::match_wheel(&bindings, &held, Wheel::Up, "", false),
            Some(&Action::Shell("workspace.switch 2".to_owned()))
        );
    }

    /// The shell's `output.configure` used to call `change_current_state` with
    /// the mode and nothing else. On udev that moves what every client is told
    /// and leaves the CRTC scanning out whatever it was driving, so the panel
    /// mode dropdown, `viewport msg -t output.configure --mode ...` and a
    /// config-reload mode change were all silent no-ops on real hardware.
    ///
    /// The fix is that this path delegates to the same `set_output_mode` that
    /// wlr-output-management uses. A DRM rig is not available in a unit test,
    /// so — like the lock-surface walks in `state/frame_barriers.rs` — the
    /// invariant is checked against the source the path is written in.
    #[test]
    fn output_configure_programs_a_mode_through_the_shared_crtc_path() {
        let source = include_str!("apply.rs");
        let start = source
            .find("fn output_configure(state: &mut ViewportState, config: OutputConfigure) {")
            .expect("output_configure in apply.rs");
        let rest = &source[start..];
        let end = rest.find("\n}\n").expect("the end of output_configure");
        let body = &rest[..end];

        assert!(
            body.contains("state.set_output_mode(&output, mode)"),
            "output.configure must program a mode through ViewportState::set_output_mode"
        );
        assert!(
            body.contains("change_current_state(None"),
            "the mode travels through set_output_mode; change_current_state must not carry it again"
        );

        let outputs = include_str!("state/outputs.rs");
        assert!(
            outputs.contains("pub(crate) fn set_output_mode("),
            "set_output_mode has to be reachable from apply.rs"
        );
    }

    /// A save during the revert countdown must not write the mode or scale the
    /// revert is about to take back. The panel disables Save while it is
    /// confirming, but `config.save` is a plain IPC message and can arrive from
    /// the shell or the CLI at any point in the twelve seconds.
    #[test]
    fn saving_omits_heads_waiting_on_confirmation() {
        use crate::output_management::HeadChange;

        let reverting = [HeadChange {
            name: "DP-1".to_owned(),
            enabled: true,
            ..Default::default()
        }];
        assert!(head_is_unconfirmed(Some(&reverting), "DP-1"));
        // A head the pending revert does not cover is still saved; the
        // countdown is about one configuration, not a freeze on saving.
        assert!(!head_is_unconfirmed(Some(&reverting), "HDMI-A-1"));
        // No countdown, nothing withheld.
        assert!(!head_is_unconfirmed(None, "DP-1"));
    }

    /// `release+` is Hyprland's `bindr` and the binding half of the same
    /// feature lives in `binding.rs`; this is the runtime-bind half. A chord
    /// matched on every field but `release` made `Mod4+q` and
    /// `release+Mod4+q` the same binding, so adding the release half silently
    /// deleted the press half (and vice versa).
    #[test]
    fn a_release_binding_does_not_replace_its_press_half() {
        let press = crate::binding::parse("Mod4+q=close").unwrap();
        let release = crate::binding::parse("release+Mod4+q=close").unwrap();
        assert!(
            !same_chord(&press, &release),
            "the two halves of a key must not compare as the same chord"
        );
        assert!(same_chord(&release, &release.clone()));

        let mut bindings = Vec::new();
        assert!(install_binding(&mut bindings, "Mod4+q", "close"));
        assert!(install_binding(
            &mut bindings,
            "release+Mod4+q",
            "shell layout.focus left"
        ));
        assert_eq!(
            bindings.len(),
            2,
            "registering the release half replaced the press half"
        );

        // Re-registering one half replaces that half alone.
        assert!(install_binding(
            &mut bindings,
            "release+Mod4+q",
            "shell layout.focus right"
        ));
        assert_eq!(bindings.len(), 2);
    }
}
