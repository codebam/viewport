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
    find_popup_root_surface, get_popup_toplevel_coords, PopupKind, Window,
};
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
        let window = Window::new_wayland_window(surface);
        let id = self.views.insert(window);
        tracing::debug!("new toplevel, view {id}");
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let Some(view) = self.views.find_by_surface(surface.wl_surface()) else {
            return;
        };
        let id = view.id;
        let window = view.window.clone();
        let announced = view.mapped;

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
        self.unconstrain_popup(&surface);
        let _ = self.popups.track_popup(PopupKind::Xdg(surface));
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

    fn grab(&mut self, _surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: Serial) {
        // Popup grabs are not ported yet.
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

    fn notify_props(&mut self, surface: &WlSurface) {
        let Some(view) = self.views.find_by_surface(surface) else {
            return;
        };
        // Before the window is announced there is nothing to update.
        if !view.mapped {
            return;
        }
        let event = Event::ViewProps {
            id: view.id,
            title: view.title(),
            app_id: view.app_id(),
        };
        self.notify(&event);
    }

    fn unconstrain_popup(&self, popup: &PopupSurface) {
        let Ok(root) = find_popup_root_surface(&PopupKind::Xdg(popup.clone())) else {
            return;
        };
        let Some(view) = self.views.find_by_surface(&root) else {
            return;
        };
        let Some(output) = self.space.outputs().next() else {
            return;
        };
        let Some(output_geo) = self.space.output_geometry(output) else {
            return;
        };
        let Some(window_geo) = self.space.element_geometry(&view.window) else {
            return;
        };

        // The positioner's target is relative to the parent's geometry.
        let mut target = output_geo;
        target.loc -= get_popup_toplevel_coords(&PopupKind::Xdg(popup.clone()));
        target.loc -= window_geo.loc;

        popup.with_pending_state(|state| {
            state.geometry = state.positioner.get_unconstrained_geometry(target);
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
                }
            }
            PopupKind::InputMethod(_) => {}
        }
    }
}
