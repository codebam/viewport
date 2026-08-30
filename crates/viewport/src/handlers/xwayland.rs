// SPDX-License-Identifier: GPL-3.0-or-later
//
// X11 applications, through Xwayland. Ports src/xwayland.c.
//
// An X11 window reaches the shell as an ordinary view: same `view.added`, same
// `view.layout` back, same rectangle. That is the whole point — the shell
// draws a frame around a DOM hole and does not need to know which protocol the
// thing inside it speaks.
//
// Nothing here scales, and that is not an omission. Every rectangle in this
// file is a logical one, the same coordinate space the shell speaks; when
// `xwayland.scale` is set, the X server's own pixels are a fraction of a
// logical pixel and the conversion happens on the way through smithay's
// window manager, in both directions — a geometry read off an X window is
// divided by the scale before it reaches `configure_request` here, and a
// rectangle handed to `X11Surface::configure` is multiplied on the way out.
// Doing the arithmetic again here would square it. See
// `ViewportState::start_xwayland` for the two halves of the setting and
// `docs/protocols.md` for what it costs.
//
// Where X11 differs is who decides. An X client tells the server where it
// wants to be and expects that to happen, so `configure_request` has to be
// answered rather than ignored; the answer is the rectangle the shell gave it,
// which may not be what was asked for. Override-redirect windows — menus,
// tooltips, drag icons — are the exception: they place themselves, are never
// announced, and are drawn where they say.

use smithay::backend::input::InputTime;
use smithay::desktop::Window;
use smithay::input::pointer::Focus;
use smithay::utils::{Logical, Rectangle, SERIAL_COUNTER};
use smithay::wayland::seat::WaylandFocus as _;
use smithay::wayland::selection::SelectionTarget;
use smithay::xwayland::xwm::{Reorder, ResizeEdge, XwmId};
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
        // `_NET_WM_PID` is supplied by the X client and is not proof of process
        // ownership. Leave identity absent rather than enabling unsafe swallow.
        let id = self.views.insert(Window::new_x11_window(window), None);
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

        if self.pointer_drag.as_ref().is_some_and(|drag| drag.id == id) {
            if self
                .pointer_drag
                .as_ref()
                .is_some_and(|drag| drag.client_requested)
            {
                if let Some(pointer) = self.seat.get_pointer() {
                    pointer.unset_grab(self, SERIAL_COUNTER.next_serial(), InputTime::now());
                }
            } else {
                self.finish_pointer_drag();
            }
        }

        self.space.unmap_elem(&element);
        self.views.remove(id);

        // The seat keeps a dead surface as its keyboard focus unless it is
        // told otherwise, and keystrokes then go nowhere until something is
        // clicked — which reads as the keyboard being dead.
        if let Some(keyboard) = self.seat.get_keyboard() {
            let was_focused = keyboard
                .current_focus()
                .zip(crate::keyboard_focus::KeyboardFocus::for_window(&element))
                .map(|(current, target)| current == target)
                .unwrap_or(false);
            if was_focused {
                let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                keyboard.set_focus(
                    self,
                    Option::<crate::keyboard_focus::KeyboardFocus>::None,
                    serial,
                );
            }
        }

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

    fn resize_request(&mut self, _xwm: XwmId, window: X11Surface, button: u32, edges: ResizeEdge) {
        let (edge, edges) = x11_resize_edges(edges);
        self.start_x11_drag(
            window,
            button,
            crate::state::DragKind::Resize,
            edges,
            Some(edge),
        );
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

    fn maximize_request(&mut self, _xwm: XwmId, window: X11Surface) {
        self.answer_x11_maximized(&window, true);
    }

    fn unmaximize_request(&mut self, _xwm: XwmId, window: X11Surface) {
        self.answer_x11_maximized(&window, false);
    }

    fn minimize_request(&mut self, _xwm: XwmId, window: X11Surface) {
        self.answer_x11_minimized(&window, true);
    }

    fn unminimize_request(&mut self, _xwm: XwmId, window: X11Surface) {
        self.answer_x11_minimized(&window, false);
    }

    fn move_request(&mut self, _xwm: XwmId, window: X11Surface, button: u32) {
        self.start_x11_drag(
            window,
            button,
            crate::state::DragKind::Move,
            (false, false),
            None,
        );
    }

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
                    .map(|surface| focus.is_surface(&surface))
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
            // Tagged as the X side's, so that a Wayland client pasting is
            // answered by asking the XWM rather than by handing it a
            // clipboard entry that has nothing to do with this selection.
            SelectionTarget::Clipboard => set_data_device_selection(
                &dh,
                &self.seat,
                mime_types,
                crate::clipboard::Owner::Xwayland,
            ),
            SelectionTarget::Primary => set_primary_selection(
                &dh,
                &self.seat,
                mime_types,
                crate::clipboard::Owner::Xwayland,
            ),
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
            // Only when the selection is still the X side's. The old test was
            // "any owner at all", and an X client exiting after the user had
            // pasted from the history — which makes the compositor itself the
            // owner — wiped a live selection it had never owned.
            SelectionTarget::Clipboard => {
                if current_data_device_selection_userdata(&self.seat)
                    .map(|owner| *owner == crate::clipboard::Owner::Xwayland)
                    .unwrap_or(false)
                {
                    clear_data_device_selection(&dh, &self.seat);
                }
            }
            SelectionTarget::Primary => {
                if current_primary_selection_userdata(&self.seat)
                    .map(|owner| *owner == crate::clipboard::Owner::Xwayland)
                    .unwrap_or(false)
                {
                    clear_primary_selection(&dh, &self.seat);
                }
            }
        }
    }
}

impl ViewportState {
    /// Start a pointer operation requested through `_NET_WM_MOVERESIZE`.
    ///
    /// X11 supplies no Wayland serial here. The active implicit grab is the
    /// authority instead: its focus and Linux button must exactly match the X
    /// window and raw X button named by the request. This prevents an X client
    /// from moving another window or manufacturing a drag without a press.
    fn start_x11_drag(
        &mut self,
        window: X11Surface,
        button: u32,
        kind: crate::state::DragKind,
        edges: (bool, bool),
        edge: Option<&'static str>,
    ) {
        if self.pointer_drag.is_some() {
            return;
        }
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let Some(start_data) = pointer.grab_start_data() else {
            return;
        };
        if !x11_button_matches(button, start_data.button) {
            return;
        }
        let Some(surface) = window.wl_surface() else {
            return;
        };
        if !start_data
            .focus
            .as_ref()
            .is_some_and(|(focus, _)| focus == &surface)
        {
            return;
        }
        let Some(id) = self
            .views
            .find_by_surface(&surface)
            .filter(|view| {
                view.mapped && !view.minimized && view.window.x11_surface() == Some(&window)
            })
            .map(|view| view.id)
        else {
            return;
        };

        self.pointer_drag = Some(crate::state::PointerDrag {
            id,
            button: start_data.button,
            kind,
            edges,
            edge,
            last: pointer.current_location(),
            pending: (0.0, 0.0),
            sent: None,
            client_requested: true,
        });
        pointer.set_grab(
            self,
            super::xdg_shell::ClientDragGrab::x11(start_data),
            SERIAL_COUNTER.next_serial(),
            Focus::Clear,
        );
    }

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
        self.foreign_management_state.set_state(
            id,
            self.focused == id,
            self.view_is_maximized(id),
            self.view_is_minimized(id),
            fullscreen,
        );
        if mapped {
            self.notify_fullscreen(id, fullscreen);
        }
    }

    fn answer_x11_maximized(&mut self, window: &X11Surface, maximized: bool) {
        if let Err(e) = window.set_maximized(maximized) {
            tracing::warn!("could not set maximized on an X11 window: {e}");
            return;
        }
        let Some(surface) = window.wl_surface() else {
            return;
        };
        let Some(view) = self.views.find_by_surface(&surface) else {
            return;
        };
        let (id, mapped) = (view.id, view.mapped);
        self.foreign_management_state.set_state(
            id,
            self.focused == id,
            maximized,
            self.view_is_minimized(id),
            self.view_is_fullscreen(id),
        );
        if mapped {
            self.notify_maximized(id, maximized);
        }
    }

    fn answer_x11_minimized(&mut self, window: &X11Surface, minimized: bool) {
        let Some(surface) = window.wl_surface() else {
            if let Err(e) = window.set_hidden(minimized) {
                tracing::warn!("could not set hidden state on an X11 window: {e}");
            }
            return;
        };
        let Some(view) = self.views.find_by_surface(&surface) else {
            return;
        };
        let (id, mapped) = (view.id, view.mapped);
        crate::apply::set_view_minimized(self, id, minimized);
        if mapped {
            self.notify_minimized(id, minimized);
        }
    }
}

/// Compare `_NET_WM_MOVERESIZE`'s X button number with Smithay's Linux code.
fn x11_button_matches(x11: u32, linux: u32) -> bool {
    let converted = match x11 {
        1 => Some(0x110), // BTN_LEFT
        2 => Some(0x112), // BTN_MIDDLE
        3 => Some(0x111), // BTN_RIGHT
        // X buttons 4-7 are wheel directions, not held pointer buttons.
        8.. => x11.checked_sub(8).and_then(|n| n.checked_add(0x113)),
        _ => None,
    };
    converted == Some(linux)
}

fn x11_resize_edges(edge: ResizeEdge) -> (&'static str, (bool, bool)) {
    match edge {
        ResizeEdge::Top => ("top", (false, true)),
        ResizeEdge::TopLeft => ("top-left", (true, true)),
        ResizeEdge::TopRight => ("top-right", (false, true)),
        ResizeEdge::Bottom => ("bottom", (false, false)),
        ResizeEdge::BottomLeft => ("bottom-left", (true, false)),
        ResizeEdge::BottomRight => ("bottom-right", (false, false)),
        ResizeEdge::Left => ("left", (true, false)),
        ResizeEdge::Right => ("right", (false, false)),
    }
}

#[cfg(test)]
mod drag_tests {
    use super::*;

    #[test]
    fn x11_resize_edges_preserve_all_requested_directions() {
        assert_eq!(x11_resize_edges(ResizeEdge::Top), ("top", (false, true)));
        assert_eq!(
            x11_resize_edges(ResizeEdge::TopLeft),
            ("top-left", (true, true))
        );
        assert_eq!(
            x11_resize_edges(ResizeEdge::TopRight),
            ("top-right", (false, true))
        );
        assert_eq!(
            x11_resize_edges(ResizeEdge::Bottom),
            ("bottom", (false, false))
        );
        assert_eq!(
            x11_resize_edges(ResizeEdge::BottomLeft),
            ("bottom-left", (true, false))
        );
        assert_eq!(
            x11_resize_edges(ResizeEdge::BottomRight),
            ("bottom-right", (false, false))
        );
        assert_eq!(x11_resize_edges(ResizeEdge::Left), ("left", (true, false)));
        assert_eq!(
            x11_resize_edges(ResizeEdge::Right),
            ("right", (false, false))
        );
    }

    #[test]
    fn x11_buttons_match_only_held_linux_buttons() {
        assert!(x11_button_matches(1, 0x110));
        assert!(x11_button_matches(2, 0x112));
        assert!(x11_button_matches(3, 0x111));
        assert!(x11_button_matches(8, 0x113));
        assert!(x11_button_matches(9, 0x114));
        assert!(!x11_button_matches(0, 0x110));
        assert!(!x11_button_matches(4, 0x113));
        assert!(!x11_button_matches(1, 0x111));
        assert!(!x11_button_matches(u32::MAX, 0x110));
    }
}
