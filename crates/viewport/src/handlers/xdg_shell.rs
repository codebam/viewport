// SPDX-License-Identifier: GPL-3.0-or-later
//
// xdg-shell. Ports src/xdg_shell.c.
//
// Note what is missing: there are no move or resize grabs. In Viewport the
// shell owns every rectangle, a window frame is DOM, and dragging an edge is
// the browser resizing a flex container — so a client asking the compositor to
// move or resize it has asked the wrong party. Those requests are ignored
// rather than implemented.

use smithay::desktop::{
    find_popup_root_surface, get_popup_toplevel_coords, PopupKeyboardGrab, PopupKind,
    PopupPointerGrab, PopupUngrabStrategy, Window,
};
use smithay::input::pointer::Focus;
use smithay::input::Seat;
use smithay::reexports::wayland_server::protocol::wl_seat;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::Serial;
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::xdg::decoration::XdgDecorationHandler;
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
    XdgToplevelSurfaceData,
};
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;

use viewport_ipc::Event;

use crate::state::ViewportState;
use crate::views::NO_VIEW;

/// xdg-dialog-v1.
///
/// Nothing to do on the change itself: the hint is kept in the toplevel's role
/// attributes by Smithay, and it is read when a window is first laid out. What
/// this trait being implemented buys is the global existing at all — without it
/// no client can set the hint, and a dialog is only ever inferred from having a
/// parent.
impl smithay::wayland::shell::xdg::dialog::XdgDialogHandler for ViewportState {}

impl XdgShellHandler for ViewportState {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    /// A window exists, but nothing is told about it yet.
    ///
    /// It is deliberately not mapped into the `Space`: the compositor has no
    /// layout policy, so until the shell answers `view.added` with a
    /// `view.layout` there is no rectangle this window could legitimately
    /// occupy. The announcement itself waits for the first buffer — see
    /// `announce_if_newly_mapped`.
    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        // The desktop itself, when it is being drawn by a process rather than
        // by an engine inside this one. It is not a window: it is not tiled,
        // not announced to the shell as something to lay out, not listed in a
        // taskbar and not offered as a screen-share source.
        if self.is_shell_client(surface.wl_surface()) {
            self.adopt_shell_toplevel(surface);
            return;
        }
        // The wallpaper terminal, for the same reasons and one more: a window
        // can be focused, and this one must never be. Registering it as a view
        // is the only thing that would make a keystroke reachable — see
        // `crate::background`.
        if self.is_background_client(surface.wl_surface()) {
            self.adopt_background_toplevel(surface);
            return;
        }
        let window = Window::new_wayland_window(surface);
        let id = self.views.insert(window);
        tracing::debug!("new toplevel, view {id}");
    }

    /// A client asking to go fullscreen — a game starting, a video going
    /// full-screen.
    ///
    /// Answered whether or not it is honoured, as in `src/xdg_shell.c:233`:
    /// the protocol requires a configure in reply, and a client that never
    /// gets one waits for it. A game that asked at startup and was ignored
    /// comes up in a window, which is what this build did — there was no
    /// handler at all, so the request reached nothing.
    ///
    /// The layout itself is the shell's, so the state goes there as the same
    /// command C sends and comes back as an ordinary rectangle.
    fn fullscreen_request(
        &mut self,
        surface: ToplevelSurface,
        _output: Option<smithay::reexports::wayland_server::protocol::wl_output::WlOutput>,
    ) {
        self.answer_fullscreen(&surface, true);
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        self.answer_fullscreen(&surface, false);
    }

    /// Maximise, which this compositor has no notion of.
    ///
    /// The shell owns the layout and there is no maximised state for it to
    /// mean. A configure is still sent, because the protocol requires an
    /// answer and a client that gets none waits for one
    /// (`src/xdg_shell.c:115`).
    fn maximize_request(&mut self, surface: ToplevelSurface) {
        surface.send_configure();
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        surface.send_configure();
    }

    fn minimize_request(&mut self, surface: ToplevelSurface) {
        surface.send_configure();
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        if self.shell_toplevel_destroyed(surface.wl_surface()) {
            return;
        }
        if self.background_toplevel_destroyed(surface.wl_surface()) {
            return;
        }
        let Some(view) = self.views.find_by_surface(surface.wl_surface()) else {
            return;
        };
        let id = view.id;
        let window = view.window.clone();
        let announced = view.mapped;
        // Or a taskbar keeps the entry for ever: nothing else tells it the
        // window is gone.
        if let Some(foreign) = view.foreign.as_ref() {
            foreign.send_closed();
        }
        // The same on the older protocol, or a taskbar keeps a window that is
        // gone and clicking it does nothing.
        self.foreign_management_state.remove(id);

        self.space.unmap_elem(&window);
        self.views.remove(id);

        // A window the shell was never told about does not need removing.
        if announced {
            self.notify(&Event::ViewRemoved { id });
        }
        if self.focused == id {
            self.notify_focus(NO_VIEW);
        }
    }

    fn title_changed(&mut self, surface: ToplevelSurface) {
        self.notify_props(surface.wl_surface());
    }

    fn app_id_changed(&mut self, surface: ToplevelSurface) {
        self.notify_props(surface.wl_surface());
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        tracing::debug!("popup: created");
        self.unconstrain_popup(&surface);
        // Not discarded: a popup that fails to be tracked is in no manager, so
        // nothing draws it and nothing finds it under the pointer — and the
        // client is told nothing either, so the menu simply never appears.
        if let Err(e) = self.popups.track_popup(PopupKind::Xdg(surface)) {
            tracing::warn!("popup: could not be tracked: {e}");
        }
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
            state.positioner = positioner;
        });
        self.unconstrain_popup(&surface);
        surface.send_repositioned(token);
    }

    /// A menu taking the pointer and the keyboard while it is open.
    ///
    /// Without this a menu opens and the click that would have chosen an entry
    /// goes to whatever is underneath, which dismisses it: a Firefox menu that
    /// appears and cannot be used. The grab is also what closes a menu when
    /// something else is clicked, and what makes Escape reach it.
    fn grab(&mut self, surface: PopupSurface, seat: wl_seat::WlSeat, serial: Serial) {
        let Some(seat) = Seat::<Self>::from_resource(&seat) else {
            return;
        };
        let kind = PopupKind::Xdg(surface);
        let Ok(root) = find_popup_root_surface(&kind) else {
            tracing::debug!("popup: grab refused, no root surface");
            return;
        };

        let grab = self.popups.grab_popup(root.into(), kind, &seat, serial);
        let mut grab = match grab {
            Ok(grab) => grab,
            Err(e) => {
                tracing::debug!("popup: grab refused: {e}");
                return;
            }
        };
        tracing::debug!("popup: grabbed");

        // A grab is only allowed to follow the one it was asked for. A client
        // grabbing from a serial that belongs to somebody else's grab would
        // take the pointer away from a menu that is already open — which is
        // how a misbehaving client freezes a desktop.
        if let Some(keyboard) = seat.get_keyboard() {
            if keyboard.is_grabbed()
                && !(keyboard.has_grab(serial)
                    || keyboard.has_grab(grab.previous_serial().unwrap_or(serial)))
            {
                grab.ungrab(PopupUngrabStrategy::All);
                return;
            }
            keyboard.set_focus(self, grab.current_grab(), serial);
            keyboard.set_grab(self, PopupKeyboardGrab::new(&grab), serial);
        }

        if let Some(pointer) = seat.get_pointer() {
            if pointer.is_grabbed()
                && !(pointer.has_grab(serial)
                    || pointer.has_grab(grab.previous_serial().unwrap_or(grab.serial())))
            {
                grab.ungrab(PopupUngrabStrategy::All);
                return;
            }
            // Focus kept: the pointer stays on whatever it was over, because a
            // menu grabbing focus to itself would send a leave to the window
            // that opened it and some clients close the menu on that.
            pointer.set_grab(self, PopupPointerGrab::new(&grab), serial, Focus::Keep);
        }
    }
}

/// Server-side decorations, always.
///
/// The shell draws every window frame in DOM, so a client titlebar is a
/// duplicate — and a client that draws its own frame reports a surface taller
/// than the rectangle the shell asked for, which overflows the slot rather
/// than filling it. C asks for the same thing (`src/main.c:64`, matching
/// sway).
///
/// A client is free to insist on drawing its own; the protocol allows it and
/// nothing here can stop it. Asking is all the protocol offers.
impl XdgDecorationHandler for ViewportState {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        let mode = self.decoration_mode();
        answer_decoration(&toplevel, mode);
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, _mode: DecorationMode) {
        // The answer does not depend on what was asked for.
        let mode = self.decoration_mode();
        answer_decoration(&toplevel, mode);
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        let mode = self.decoration_mode();
        answer_decoration(&toplevel, mode);
    }
}

fn answer_decoration(toplevel: &ToplevelSurface, mode: DecorationMode) {
    toplevel.with_pending_state(|state| {
        state.decoration_mode = Some(mode);
    });
    toplevel.send_configure();
}

impl ViewportState {
    /// Server side unless the config file says `"decorations": "client"`.
    fn decoration_mode(&self) -> DecorationMode {
        if self.server_decorations {
            DecorationMode::ServerSide
        } else {
            DecorationMode::ClientSide
        }
    }

    pub(crate) fn notify_props(&mut self, surface: &WlSurface) {
        let Some(view) = self.views.find_by_surface(surface) else {
            return;
        };
        // Before the window is announced there is nothing to update.
        if !view.mapped {
            return;
        }
        let (title, app_id) = (view.title(), view.app_id());
        // The outside list carries the same change; a taskbar showing a stale
        // title is the same bug as a dock showing one.
        if let Some(foreign) = view.foreign.as_ref() {
            foreign.send_title(&title);
            foreign.send_app_id(&app_id);
            foreign.send_done();
        }
        let id = view.id;
        self.foreign_management_state.update(id, &title, &app_id);
        let icon = view.icon.clone();
        let event = Event::ViewProps {
            id,
            title,
            app_id,
            icon,
        };
        self.notify(&event);
    }

    /// Set the state a client asked for and tell the shell.
    fn answer_fullscreen(&mut self, surface: &ToplevelSurface, fullscreen: bool) {
        surface.with_pending_state(|pending| {
            if fullscreen {
                pending.states.set(
                    smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Fullscreen,
                );
            } else {
                pending.states.unset(
                    smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Fullscreen,
                );
            }
        });
        surface.send_configure();

        let Some(view) = self.views.find_by_surface(surface.wl_surface()) else {
            return;
        };
        let (id, mapped) = (view.id, view.mapped);
        self.foreign_management_state
            .set_state(id, self.focused == id, fullscreen);
        // Only once the shell knows the window exists.
        //
        // A client is allowed to ask for fullscreen before its first commit —
        // mpv --fullscreen and most games do — and that is well before the
        // window is announced. The command went out anyway, naming a view the
        // shell had never heard of, so it was dropped; then `view.added`
        // arrived carrying no fullscreen state and the window opened in a
        // frame. The announce path re-sends this once there is something to
        // send it about.
        if mapped {
            self.notify_fullscreen(id, fullscreen);
        }
    }

    /// Tell the shell a window's fullscreen state.
    ///
    /// The layout is the shell's, so this is the whole of what the compositor
    /// does about fullscreen — the rectangle comes back as an ordinary
    /// `view.layout`. Shared by the xdg and X11 paths so both windows mean the
    /// same thing by it.
    pub(crate) fn notify_fullscreen(&mut self, id: u32, fullscreen: bool) {
        self.notify(&Event::ShellCommand {
            command: "window.fullscreen.set".to_owned(),
            args: vec![id.to_string(), u8::from(fullscreen).to_string()],
        });
    }

    fn unconstrain_popup(&self, popup: &PopupSurface) {
        let Ok(root) = find_popup_root_surface(&PopupKind::Xdg(popup.clone())) else {
            return;
        };
        let Some(view) = self.views.find_by_surface(&root) else {
            return;
        };
        // The output this window is on, not the first one there is.
        //
        // The positioner slides a menu until it fits the rectangle it is
        // given. Handing it the first output's rectangle while the window sits
        // on the second describes a screen that ends thousands of pixels to
        // the left of the window, so the menu was pushed off the left edge to
        // "fit" — configured at -320,32 and invisible, which is a menu that
        // does not open.
        // The output holding most of the window, by area.
        //
        // Not the first one the window touches: a client with client-side
        // decorations draws shadows outside its window, so a window ten
        // pixels inside the right-hand monitor overlaps the left-hand one by
        // the width of its shadow and was described by the wrong screen — the
        // target came out as -2570,-10 and the positioner slid a menu that
        // had asked for 909,32 off to -320,32 to fit it.
        let window_rect = self
            .space
            .element_geometry(&view.window)
            .unwrap_or_default();
        let output = self
            .space
            .outputs()
            .max_by_key(|output| {
                self.space
                    .output_geometry(output)
                    .and_then(|geometry| geometry.intersection(window_rect))
                    .map(|shared| shared.size.w as i64 * shared.size.h as i64)
                    .unwrap_or(0)
            })
            .cloned();
        let Some(output) = output else {
            return;
        };
        let Some(output_geo) = self.space.output_geometry(&output) else {
            return;
        };
        let Some(window_geo) = self.space.element_geometry(&view.window) else {
            return;
        };

        // The positioner's target is relative to the parent's geometry.
        let parent_offset = get_popup_toplevel_coords(&PopupKind::Xdg(popup.clone()));
        let mut target = output_geo;
        target.loc -= parent_offset;
        target.loc -= window_geo.loc;

        popup.with_pending_state(|state| {
            let asked = state.positioner.get_geometry();
            state.geometry = state.positioner.get_unconstrained_geometry(target);
            // Everything the placement is decided from, because "the menu is
            // in the wrong place" has three possible authors: the client's
            // positioner, the rectangle handed to it, or the sliding done to
            // fit.
            tracing::debug!(
                "popup: anchor {:?} {:?} gravity {:?} asked {},{} {}x{} \
                 in target {},{} {}x{} (window at {},{}, parent offset {},{}) \
                 becomes {},{}",
                state.positioner.anchor_rect,
                state.positioner.anchor_edges,
                state.positioner.gravity,
                asked.loc.x,
                asked.loc.y,
                asked.size.w,
                asked.size.h,
                target.loc.x,
                target.loc.y,
                target.size.w,
                target.size.h,
                window_geo.loc.x,
                window_geo.loc.y,
                parent_offset.x,
                parent_offset.y,
                state.geometry.loc.x,
                state.geometry.loc.y,
            );
        });
    }
}

/// Send the initial configure a client is waiting on before it will paint.
pub fn handle_commit(state: &mut ViewportState, surface: &WlSurface) {
    if let Some(view) = state.views.find_by_surface(surface) {
        if let Some(toplevel) = view.window.toplevel() {
            let sent = with_states(surface, |states| {
                states
                    .data_map
                    .get::<XdgToplevelSurfaceData>()
                    .map(|data| data.lock().unwrap().initial_configure_sent)
                    .unwrap_or(true)
            });
            if !sent {
                toplevel.send_configure();
            }
        }
    }

    state.popups.commit(surface);
    if let Some(popup) = state.popups.find_popup(surface) {
        match popup {
            PopupKind::Xdg(ref xdg) => {
                if !xdg.is_initial_configure_sent() {
                    // The initial configure is always allowed, so this cannot
                    // legitimately fail.
                    xdg.send_configure().expect("initial configure failed");
                    let geometry = xdg.with_pending_state(|state| state.geometry);
                    tracing::debug!(
                        "popup: configured at {},{} {}x{}",
                        geometry.loc.x,
                        geometry.loc.y,
                        geometry.size.w,
                        geometry.size.h
                    );
                }
            }
            PopupKind::InputMethod(_) => {}
        }
    }
}
