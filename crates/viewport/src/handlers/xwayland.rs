// SPDX-License-Identifier: GPL-3.0-or-later
//
// X11 applications, through Xwayland. Ports src/xwayland.c.
//
// An X11 window reaches the shell as an ordinary view: same `view.added`, same
// `view.layout` back, same rectangle. That is the whole point — the shell
// draws a frame around a DOM hole and does not need to know which protocol the
// thing inside it speaks.
//
// Where X11 differs is who decides. An X client tells the server where it
// wants to be and expects that to happen, so `configure_request` has to be
// answered rather than ignored; the answer is the rectangle the shell gave it,
// which may not be what was asked for. Override-redirect windows — menus,
// tooltips, drag icons — are the exception: they place themselves, are never
// announced, and are drawn where they say.

use smithay::desktop::Window;
use smithay::utils::{Logical, Rectangle};
use smithay::wayland::seat::WaylandFocus as _;
use smithay::wayland::selection::SelectionTarget;
use smithay::xwayland::xwm::{Reorder, XwmId};
use smithay::xwayland::{X11Surface, X11Wm, XwmHandler};

use viewport_ipc::Event;

use crate::state::ViewportState;

/// The xwayland-shell protocol, which is how Xwayland tells the compositor
/// that a wl_surface belongs to a given X window. Nothing to decide here; the
/// state is the whole of it.
impl smithay::wayland::xwayland_shell::XWaylandShellHandler for ViewportState {
    fn xwayland_shell_state(
        &mut self,
    ) -> &mut smithay::wayland::xwayland_shell::XWaylandShellState {
        &mut self.xwayland_shell_state
    }
}

impl XwmHandler for ViewportState {
    fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
        self.xwm.as_mut().expect("the window manager is running")
    }

    fn new_window(&mut self, _xwm: XwmId, _window: X11Surface) {
        // Nothing yet. As with xdg-shell, a window with no buffer has no title
        // and no size worth telling the shell about.
    }

    fn new_override_redirect_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    /// The client asked to be shown.
    ///
    /// The rectangle it wants is not the compositor's to grant — the shell
    /// decides — but a mapped X window must have some geometry or the client
    /// waits forever. It gets what it asked for now and the shell's answer a
    /// moment later, which is the same order a Wayland client sees.
    fn map_window_request(&mut self, _xwm: XwmId, window: X11Surface) {
        let geometry = window.geometry();
        if let Err(e) = window.set_mapped(true) {
            tracing::warn!("could not map an X11 window: {e}");
            return;
        }
        if let Err(e) = window.configure(geometry) {
            tracing::warn!("could not configure an X11 window: {e}");
        }
        let id = self.views.insert(Window::new_x11_window(window));
        tracing::debug!("new X11 window, view {id}");
    }

    /// A menu or a tooltip, which places itself.
    ///
    /// Never announced to the shell: it has no frame, no slot and no place in
    /// the layout, and telling the shell about it would put a window frame
    /// around a dropdown.
    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
        let geometry = window.geometry();
        let element = Window::new_x11_window(window);
        // Mapped without activating. Activating here does not focus the menu —
        // it takes the activated state off every other window, into a pending
        // configure that goes out with whatever is sent next, so the window the
        // menu belongs to greys out while its own menu is open.
        self.space.map_element(element, geometry.loc, false);
        self.needs_render = true;
    }

    fn unmapped_window(&mut self, _xwm: XwmId, window: X11Surface) {
        let Some(surface) = window.wl_surface() else {
            return;
        };
        let Some(view) = self.views.find_by_surface(&surface) else {
            // Override-redirect: in the space but not in the registry.
            let element = self
                .space
                .elements()
                .find(|element| element.x11_surface() == Some(&window))
                .cloned();
            if let Some(element) = element {
                self.space.unmap_elem(&element);
                self.needs_render = true;
            }
            return;
        };
        let (id, element, announced) = (view.id, view.window.clone(), view.mapped);
        if let Some(foreign) = view.foreign.as_ref() {
            foreign.send_closed();
        }
        self.foreign_management_state.remove(view.id);

        self.space.unmap_elem(&element);
        self.views.remove(id);
        if announced {
            self.notify(&Event::ViewRemoved { id });
        }
        if self.focused == id {
            self.notify_focus(crate::views::NO_VIEW);
        }
        self.needs_render = true;
    }

    fn destroyed_window(&mut self, _xwm: XwmId, _window: X11Surface) {
        // Everything happens at unmap; a destroyed window is already gone from
        // the registry by the time this arrives.
    }

    /// An X client asking to be moved or resized.
    ///
    /// Answered rather than ignored, because an X client that gets no reply
    /// waits rather than carrying on — unlike a Wayland one, which is why the
    /// xdg-shell side ignores the equivalent. What it is told is the rectangle
    /// it already has, since only the shell may change that.
    fn configure_request(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        _x: Option<i32>,
        _y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        _reorder: Option<Reorder>,
    ) {
        let mut geometry = window.geometry();
        // An unplaced window has nothing to keep, so its own idea of its size
        // is better than a zero.
        let placed = window
            .wl_surface()
            .and_then(|surface| self.views.find_by_surface(&surface).map(|view| view.placed))
            .unwrap_or(false);
        if !placed {
            if let Some(w) = w {
                geometry.size.w = w as i32;
            }
            if let Some(h) = h {
                geometry.size.h = h as i32;
            }
        }
        if let Err(e) = window.configure(geometry) {
            tracing::warn!("could not answer an X11 configure request: {e}");
        }
    }

    /// The window moved itself, which only an override-redirect one may do.
    fn configure_notify(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        geometry: Rectangle<i32, Logical>,
        _above: Option<u32>,
    ) {
        if !window.is_override_redirect() {
            return;
        }
        let element = self
            .space
            .elements()
            .find(|element| element.x11_surface() == Some(&window))
            .cloned();
        if let Some(element) = element {
            self.space.map_element(element, geometry.loc, false);
            self.needs_render = true;
        }
    }

    fn resize_request(
        &mut self,
        _xwm: XwmId,
        _window: X11Surface,
        _button: u32,
        _edges: smithay::xwayland::xwm::ResizeEdge,
    ) {
        // No grabs, as with xdg-shell: the frame is DOM and dragging an edge
        // is the browser resizing a flex container, so a client asking the
        // compositor to resize it has asked the wrong party.
    }

    /// An X11 client asking to go fullscreen — a game, usually.
    ///
    /// `XwmHandler` gives these an empty default, so leaving them out was not
    /// neutral: every X11 fullscreen request was accepted by the trait and
    /// dropped on the floor. The window kept its frame and the client was never
    /// told otherwise, because nothing set the property back.
    ///
    /// The layout is the shell's, as on the Wayland side. What belongs here is
    /// saying yes to the client and passing the state on.
    fn fullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        self.answer_x11_fullscreen(&window, true);
    }

    fn unfullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        self.answer_x11_fullscreen(&window, false);
    }

    fn move_request(&mut self, _xwm: XwmId, _window: X11Surface, _button: u32) {}

    /// Whether an X client may read the Wayland clipboard.
    ///
    /// Only while an X window holds the keyboard. Any client being able to
    /// read the clipboard whenever it likes is how a clipboard becomes a
    /// side channel, and focus is the only evidence the compositor has that
    /// the user meant this application to have it.
    fn allow_selection_access(&mut self, _xwm: XwmId, _selection: SelectionTarget) -> bool {
        let Some(focus) = self.seat.get_keyboard().and_then(|k| k.current_focus()) else {
            return false;
        };
        self.space.elements().any(|window| {
            window.x11_surface().is_some()
                && window
                    .wl_surface()
                    .map(|surface| *surface == focus)
                    .unwrap_or(false)
        })
    }

    /// An X client is pasting: hand it the Wayland selection.
    fn send_selection(
        &mut self,
        _xwm: XwmId,
        selection: SelectionTarget,
        mime_type: String,
        fd: std::os::unix::io::OwnedFd,
    ) {
        use smithay::wayland::selection::data_device::request_data_device_client_selection;
        use smithay::wayland::selection::primary_selection::request_primary_client_selection;

        // The two return different error types, so each is reported where it
        // happens rather than unified into something less specific.
        match selection {
            SelectionTarget::Clipboard => {
                if let Err(e) = request_data_device_client_selection(&self.seat, mime_type, fd) {
                    tracing::warn!("could not hand the clipboard to Xwayland: {e}");
                }
            }
            SelectionTarget::Primary => {
                if let Err(e) = request_primary_client_selection(&self.seat, mime_type, fd) {
                    tracing::warn!("could not hand the primary selection to Xwayland: {e}");
                }
            }
        }
    }

    /// An X client copied something: offer it to Wayland clients.
    fn new_selection(&mut self, _xwm: XwmId, selection: SelectionTarget, mime_types: Vec<String>) {
        use smithay::wayland::selection::data_device::set_data_device_selection;
        use smithay::wayland::selection::primary_selection::set_primary_selection;

        let dh = self.display_handle.clone();
        match selection {
            SelectionTarget::Clipboard => {
                set_data_device_selection(&dh, &self.seat, mime_types, ())
            }
            SelectionTarget::Primary => set_primary_selection(&dh, &self.seat, mime_types, ()),
        }
    }

    /// The X client that owned the selection has gone.
    ///
    /// Only cleared if it is still ours: a Wayland client may have taken the
    /// selection since, and clearing then would throw away something the X
    /// side never owned.
    fn cleared_selection(&mut self, _xwm: XwmId, selection: SelectionTarget) {
        use smithay::wayland::selection::data_device::{
            clear_data_device_selection, current_data_device_selection_userdata,
        };
        use smithay::wayland::selection::primary_selection::{
            clear_primary_selection, current_primary_selection_userdata,
        };

        let dh = self.display_handle.clone();
        match selection {
            SelectionTarget::Clipboard => {
                if current_data_device_selection_userdata(&self.seat).is_some() {
                    clear_data_device_selection(&dh, &self.seat);
                }
            }
            SelectionTarget::Primary => {
                if current_primary_selection_userdata(&self.seat).is_some() {
                    clear_primary_selection(&dh, &self.seat);
                }
            }
        }
    }
}

impl ViewportState {
    /// Grant an X11 fullscreen request and tell the shell.
    ///
    /// The property has to be set back on the window whatever the shell then
    /// does with the layout: it is how the client learns the request was
    /// granted, and a toolkit that never sees it changed goes on drawing its
    /// own decorations over a window it believes is still framed.
    fn answer_x11_fullscreen(&mut self, window: &X11Surface, fullscreen: bool) {
        if let Err(e) = window.set_fullscreen(fullscreen) {
            tracing::warn!("could not set fullscreen on an X11 window: {e}");
            return;
        }
        let Some(surface) = window.wl_surface() else {
            // No surface yet, so no view and nothing announced. The state is
            // on the window now and `wants_fullscreen` reads it back when the
            // window is finally announced.
            return;
        };
        let Some(view) = self.views.find_by_surface(&surface) else {
            return;
        };
        let (id, mapped) = (view.id, view.mapped);
        self.foreign_management_state
            .set_state(id, self.focused == id, fullscreen);
        if mapped {
            self.notify_fullscreen(id, fullscreen);
        }
    }
}
