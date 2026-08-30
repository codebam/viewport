// SPDX-License-Identifier: GPL-3.0-or-later
//
// xdg-shell. Ports src/xdg_shell.c.
//
// The shell owns every rectangle, so client move/resize grabs forward pointer
// deltas to it rather than changing Smithay's Space directly.

use smithay::desktop::{
    find_popup_root_surface, get_popup_toplevel_coords, PopupKeyboardGrab, PopupKind,
    PopupPointerGrab, PopupUngrabStrategy, Window,
};
use smithay::backend::input::ButtonState;
use smithay::input::pointer::{
    AxisFrame, ButtonEvent, Focus, GestureHoldBeginEvent, GestureHoldEndEvent,
    GesturePinchBeginEvent, GesturePinchEndEvent, GesturePinchUpdateEvent,
    GestureSwipeBeginEvent, GestureSwipeEndEvent, GestureSwipeUpdateEvent,
    GrabStartData as PointerGrabStartData, MotionEvent, PointerGrab, PointerInnerHandle,
    RelativeMotionEvent,
};
use smithay::input::Seat;
use smithay::reexports::wayland_server::protocol::wl_seat;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::Resource as _;
use smithay::utils::{Logical, Point, Serial};
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::xdg::decoration::XdgDecorationHandler;
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
    XdgToplevelSurfaceData,
};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
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
        let process = surface
            .wl_surface()
            .client()
            .and_then(|client| client.get_credentials(&self.display_handle).ok())
            .and_then(|credentials| u32::try_from(credentials.pid).ok())
            .and_then(crate::views::ProcessIdentity::for_pid);
        let window = Window::new_wayland_window(surface);
        let id = self.views.insert(window, process);
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

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        self.answer_maximized(&surface, true);
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        self.answer_maximized(&surface, false);
    }

    fn move_request(&mut self, surface: ToplevelSurface, seat: wl_seat::WlSeat, serial: Serial) {
        self.start_xdg_drag(
            surface,
            seat,
            serial,
            crate::state::DragKind::Move,
            (false, false),
            None,
        );
    }

    fn resize_request(
        &mut self,
        surface: ToplevelSurface,
        seat: wl_seat::WlSeat,
        serial: Serial,
        edges: xdg_toplevel::ResizeEdge,
    ) {
        let Some((edge, edges)) = xdg_resize_edges(edges) else {
            return;
        };
        self.start_xdg_drag(
            surface,
            seat,
            serial,
            crate::state::DragKind::Resize,
            edges,
            Some(edge),
        );
    }

    fn minimize_request(&mut self, surface: ToplevelSurface) {
        let Some(id) = self
            .views
            .find_by_surface(surface.wl_surface())
            .map(|view| view.id)
        else {
            surface.send_configure();
            return;
        };
        let mapped = self.views.get(id).is_some_and(|view| view.mapped);
        crate::apply::set_view_minimized(self, id, true);
        // xdg-shell has no minimized state enum, but still requires an answer.
        surface.send_configure();
        if mapped {
            self.notify_minimized(id, true);
        }
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

        // The seat keeps a destroyed surface as its keyboard focus unless it
        // is told otherwise, and keystrokes then go nowhere until something
        // is clicked — which reads as the keyboard being dead.
        if let Some(keyboard) = self.seat.get_keyboard() {
            let was_focused = keyboard
                .current_focus()
                .zip(crate::keyboard_focus::KeyboardFocus::for_window(&window))
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

        // The last frame of the closed window would otherwise stay on screen
        // until unrelated damage drew over it: vblank stops when nothing is
        // submitted, and nothing submits for a window that is gone.
        self.needs_render = true;

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

    /// The window this one is a dialog of has changed, or been named at last.
    ///
    /// The second is the case that matters. A dialog an application opens
    /// itself has its parent set before it ever commits, so `view.added`
    /// carries it. A dialog that comes from somewhere else does not: a file
    /// chooser is the portal's window, in another process, and the parent
    /// arrives over xdg-foreign — an export, an import, and a `set_parent_of`
    /// — well after the window has mapped and been announced. Smithay routes
    /// that through here (`xdg_foreign::handlers`), which is why this is the
    /// only place the shell can learn it, and without it a portal dialog is a
    /// window belonging to nothing that a layout can only put in the middle of
    /// the screen and hope.
    fn parent_changed(&mut self, surface: ToplevelSurface) {
        self.notify_parent(surface.wl_surface());
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

/// KDE's older server-decoration protocol, what Plasma clients spoke before
/// xdg-decoration. Smithay implements the global, so all that is left is to
/// answer with this compositor's mode and never let a client talk us into a
/// feedback loop: `request_mode` answers with what we want, not what was
/// asked for, exactly as the xdg-decoration handler does above.
impl smithay::wayland::shell::kde::decoration::KdeDecorationHandler for ViewportState {
    fn kde_decoration_state(
        &self,
    ) -> &smithay::wayland::shell::kde::decoration::KdeDecorationState {
        &self.kde_decoration_state
    }

    fn new_decoration(
        &mut self,
        _surface: &WlSurface,
        decoration: &smithay::reexports::wayland_protocols_misc::server_decoration::server::org_kde_kwin_server_decoration::OrgKdeKwinServerDecoration,
    ) {
        answer_kde_decoration(decoration, self.decoration_mode());
    }

    fn request_mode(
        &mut self,
        _surface: &WlSurface,
        decoration: &smithay::reexports::wayland_protocols_misc::server_decoration::server::org_kde_kwin_server_decoration::OrgKdeKwinServerDecoration,
        _mode: smithay::reexports::wayland_server::WEnum<
            smithay::reexports::wayland_protocols_misc::server_decoration::server::org_kde_kwin_server_decoration::Mode,
        >,
    ) {
        // The answer does not depend on what was asked for.
        answer_kde_decoration(decoration, self.decoration_mode());
    }
}

/// The xdg and KDE enums agree numerically (Client=1, Server=2), so a single
/// conversion covers both.
fn answer_kde_decoration(
    decoration: &smithay::reexports::wayland_protocols_misc::server_decoration::server::org_kde_kwin_server_decoration::OrgKdeKwinServerDecoration,
    mode: DecorationMode,
) {
    use smithay::reexports::wayland_protocols_misc::server_decoration::server::org_kde_kwin_server_decoration::Mode as KdeMode;
    let mode = match mode {
        DecorationMode::ServerSide => KdeMode::Server,
        _ => KdeMode::Client,
    };
    decoration.mode(mode);
}

impl ViewportState {
    fn start_xdg_drag(
        &mut self,
        surface: ToplevelSurface,
        seat_resource: wl_seat::WlSeat,
        serial: Serial,
        kind: crate::state::DragKind,
        edges: (bool, bool),
        edge: Option<&'static str>,
    ) {
        let Some(seat) = Seat::<Self>::from_resource(&seat_resource) else {
            return;
        };
        let Some(pointer) = seat.get_pointer() else {
            return;
        };
        if !pointer.has_grab(serial) {
            return;
        }
        let Some(start_data) = pointer.grab_start_data() else {
            return;
        };
        let Some((focus, _)) = start_data.focus.as_ref() else {
            return;
        };
        if !focus.id().same_client_as(&surface.wl_surface().id()) {
            return;
        }
        let Some(id) = self
            .views
            .find_by_surface(surface.wl_surface())
            .filter(|view| view.mapped && !view.minimized)
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
        if kind == crate::state::DragKind::Resize {
            surface.with_pending_state(|state| {
                state.states.set(xdg_toplevel::State::Resizing);
            });
            surface.send_pending_configure();
        }
        pointer.set_grab(
            self,
            ClientDragGrab::xdg(start_data, surface, kind == crate::state::DragKind::Resize),
            serial,
            Focus::Clear,
        );
    }

    /// Server side unless the config file says `"decorations": "client"`.
    fn decoration_mode(&self) -> DecorationMode {
        if self.server_decorations {
            DecorationMode::ServerSide
        } else {
            DecorationMode::ClientSide
        }
    }

    /// Tell the shell who this window belongs to now.
    ///
    /// Nothing before it is mapped: the shell has not been told the window
    /// exists, so a message about its parent names an id it has never seen.
    /// `view.added` carries the parent for everything announced after this
    /// point, which is what makes the two paths cover the whole of it.
    pub(crate) fn notify_parent(&mut self, surface: &WlSurface) {
        let Some(view) = self.views.find_by_surface(surface) else {
            return;
        };
        if !view.mapped {
            return;
        }
        let id = view.id;
        let parent = self.views.parent_id_of(view);
        self.notify(&Event::ViewParent { id, parent });
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
        let tag = view.tag.clone();
        let event = Event::ViewProps {
            id,
            title,
            app_id,
            tag,
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
        self.foreign_management_state.set_state(
            id,
            self.focused == id,
            self.view_is_maximized(id),
            self.view_is_minimized(id),
            fullscreen,
        );
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

    fn answer_maximized(&mut self, surface: &ToplevelSurface, maximized: bool) {
        use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State;

        surface.with_pending_state(|pending| {
            if maximized {
                pending.states.set(State::Maximized);
            } else {
                pending.states.unset(State::Maximized);
            }
        });
        surface.send_configure();

        let Some(view) = self.views.find_by_surface(surface.wl_surface()) else {
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

    pub(crate) fn notify_maximized(&mut self, id: u32, maximized: bool) {
        self.notify(&Event::ShellCommand {
            command: "window.maximized.set".to_owned(),
            args: vec![id.to_string(), u8::from(maximized).to_string()],
        });
    }

    pub(crate) fn notify_minimized(&mut self, id: u32, minimized: bool) {
        self.notify(&Event::ShellCommand {
            command: "window.minimized.set".to_owned(),
            args: vec![id.to_string(), u8::from(minimized).to_string()],
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

fn xdg_resize_edges(edges: xdg_toplevel::ResizeEdge) -> Option<(&'static str, (bool, bool))> {
    use xdg_toplevel::ResizeEdge;

    match edges {
        ResizeEdge::Top => Some(("top", (false, true))),
        ResizeEdge::TopLeft => Some(("top-left", (true, true))),
        ResizeEdge::TopRight => Some(("top-right", (false, true))),
        ResizeEdge::Bottom => Some(("bottom", (false, false))),
        ResizeEdge::BottomLeft => Some(("bottom-left", (true, false))),
        ResizeEdge::BottomRight => Some(("bottom-right", (false, false))),
        ResizeEdge::Left => Some(("left", (true, false))),
        ResizeEdge::Right => Some(("right", (false, false))),
        _ => None,
    }
}

pub(crate) struct ClientDragGrab {
    start_data: PointerGrabStartData<ViewportState>,
    /// Only xdg-shell has a protocol resizing state to clear when the grab ends.
    resize_surface: Option<ToplevelSurface>,
}

impl ClientDragGrab {
    fn xdg(
        start_data: PointerGrabStartData<ViewportState>,
        surface: ToplevelSurface,
        resizing: bool,
    ) -> Self {
        Self {
            start_data,
            resize_surface: resizing.then_some(surface),
        }
    }

    pub(crate) fn x11(start_data: PointerGrabStartData<ViewportState>) -> Self {
        Self {
            start_data,
            resize_surface: None,
        }
    }
}

impl PointerGrab<ViewportState> for ClientDragGrab {
    fn motion(
        &mut self,
        data: &mut ViewportState,
        handle: &mut PointerInnerHandle<'_, ViewportState>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        handle.motion(data, None, event);
    }

    fn relative_motion(
        &mut self,
        _data: &mut ViewportState,
        _handle: &mut PointerInnerHandle<'_, ViewportState>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        _event: &RelativeMotionEvent,
    ) {
    }

    fn button(
        &mut self,
        data: &mut ViewportState,
        handle: &mut PointerInnerHandle<'_, ViewportState>,
        event: &ButtonEvent,
    ) {
        if event.state == ButtonState::Released && event.button == self.start_data.button {
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    fn axis(
        &mut self,
        _data: &mut ViewportState,
        _handle: &mut PointerInnerHandle<'_, ViewportState>,
        _details: AxisFrame,
    ) {
    }

    fn frame(
        &mut self,
        data: &mut ViewportState,
        handle: &mut PointerInnerHandle<'_, ViewportState>,
    ) {
        handle.frame(data);
    }

    fn gesture_swipe_begin(
        &mut self,
        _data: &mut ViewportState,
        _handle: &mut PointerInnerHandle<'_, ViewportState>,
        _event: &GestureSwipeBeginEvent,
    ) {
    }

    fn gesture_swipe_update(
        &mut self,
        _data: &mut ViewportState,
        _handle: &mut PointerInnerHandle<'_, ViewportState>,
        _event: &GestureSwipeUpdateEvent,
    ) {
    }

    fn gesture_swipe_end(
        &mut self,
        _data: &mut ViewportState,
        _handle: &mut PointerInnerHandle<'_, ViewportState>,
        _event: &GestureSwipeEndEvent,
    ) {
    }

    fn gesture_pinch_begin(
        &mut self,
        _data: &mut ViewportState,
        _handle: &mut PointerInnerHandle<'_, ViewportState>,
        _event: &GesturePinchBeginEvent,
    ) {
    }

    fn gesture_pinch_update(
        &mut self,
        _data: &mut ViewportState,
        _handle: &mut PointerInnerHandle<'_, ViewportState>,
        _event: &GesturePinchUpdateEvent,
    ) {
    }

    fn gesture_pinch_end(
        &mut self,
        _data: &mut ViewportState,
        _handle: &mut PointerInnerHandle<'_, ViewportState>,
        _event: &GesturePinchEndEvent,
    ) {
    }

    fn gesture_hold_begin(
        &mut self,
        _data: &mut ViewportState,
        _handle: &mut PointerInnerHandle<'_, ViewportState>,
        _event: &GestureHoldBeginEvent,
    ) {
    }

    fn gesture_hold_end(
        &mut self,
        _data: &mut ViewportState,
        _handle: &mut PointerInnerHandle<'_, ViewportState>,
        _event: &GestureHoldEndEvent,
    ) {
    }

    fn start_data(&self) -> &PointerGrabStartData<ViewportState> {
        &self.start_data
    }

    fn unset(&mut self, data: &mut ViewportState) {
        data.finish_pointer_drag();
        if let Some(surface) = self.resize_surface.as_ref() {
            surface.with_pending_state(|state| {
                state.states.unset(xdg_toplevel::State::Resizing);
            });
            surface.send_pending_configure();
        }
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

#[cfg(test)]
mod drag_tests {
    use super::*;

    #[test]
    fn xdg_resize_edges_preserve_corner_direction() {
        use xdg_toplevel::ResizeEdge::*;

        assert_eq!(xdg_resize_edges(TopLeft), Some(("top-left", (true, true))));
        assert_eq!(
            xdg_resize_edges(TopRight),
            Some(("top-right", (false, true)))
        );
        assert_eq!(
            xdg_resize_edges(BottomLeft),
            Some(("bottom-left", (true, false)))
        );
        assert_eq!(
            xdg_resize_edges(BottomRight),
            Some(("bottom-right", (false, false)))
        );
        assert_eq!(xdg_resize_edges(Top), Some(("top", (false, true))));
        assert_eq!(xdg_resize_edges(Left), Some(("left", (true, false))));
        assert_eq!(xdg_resize_edges(None), std::option::Option::None);
    }
}
