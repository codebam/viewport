// SPDX-License-Identifier: GPL-3.0-or-later
//
// Smithay protocol handler implementations.
//
// Adapted from smithay's `smallvil` example, which is MIT-licensed; this crate
// is GPL-3.0-or-later, which MIT permits.

mod compositor;
mod xdg_shell;

pub mod layer_shell;
pub mod session_lock;
pub mod xwayland;

use smithay::input::dnd::{DnDGrab, DndGrabHandler, GrabType, Source};
use smithay::input::pointer::Focus;
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::Resource;
use smithay::utils::Serial;
use smithay::wayland::output::OutputHandler;
use smithay::wayland::selection::data_device::{
    set_data_device_focus, DataDeviceHandler, DataDeviceState, WaylandDndGrabHandler,
};
use smithay::wayland::selection::SelectionHandler;

use crate::state::ViewportState;

impl SeatHandler for ViewportState {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    /// A client set its own pointer image, or asked for a named one.
    ///
    /// Kept rather than acted on: what is drawn is decided at render time,
    /// because the same status has to produce a different image on an output
    /// with a different scale.
    fn cursor_image(&mut self, _seat: &Seat<Self>, image: smithay::input::pointer::CursorImageStatus) {
        self.cursor_status = image;
        // The pointer changing shape is a visible change with no other reason
        // to draw a frame behind it.
        self.needs_render = true;
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let dh = &self.display_handle;
        let client = focused.and_then(|s| dh.get_client(s.id()).ok());
        set_data_device_focus(dh, seat, client);
    }
}

impl SelectionHandler for ViewportState {
    type SelectionUserData = ();
}

impl DataDeviceHandler for ViewportState {
    fn data_device_state(&mut self) -> &mut DataDeviceState {
        &mut self.data_device_state
    }
}

impl DndGrabHandler for ViewportState {}

impl WaylandDndGrabHandler for ViewportState {
    fn dnd_requested<S: Source>(
        &mut self,
        source: S,
        _icon: Option<WlSurface>,
        seat: Seat<Self>,
        serial: Serial,
        type_: GrabType,
    ) {
        match type_ {
            GrabType::Pointer => {
                let Some(pointer) = seat.get_pointer() else {
                    source.cancel();
                    return;
                };
                let Some(start_data) = pointer.grab_start_data() else {
                    source.cancel();
                    return;
                };
                let grab = DnDGrab::new_pointer(&self.display_handle, start_data, source, seat);
                pointer.set_grab(self, grab, serial, Focus::Keep);
            }
            // Touch is not wired up yet.
            GrabType::Touch => source.cancel(),
        }
    }
}

impl OutputHandler for ViewportState {}

impl crate::screencopy::ScreencopyHandler for ViewportState {
    fn screencopy_state(&mut self) -> &mut crate::screencopy::ScreencopyState {
        &mut self.screencopy_state
    }

    fn queue_copy(
        &mut self,
        frame: &smithay::reexports::wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
        state: &crate::screencopy::FrameState,
        buffer: &smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer,
        with_damage: bool,
    ) -> Result<(), String> {
        self.pending_copies.push(crate::state::PendingCopy {
            frame: frame.clone(),
            buffer: buffer.clone(),
            output: state.output.clone(),
            region: state.region,
            overlay_cursor: state.overlay_cursor,
            with_damage,
        });
        // The copy happens when the output is next drawn, so something has to
        // ask for a draw. An idle desktop draws nothing, and a screenshot of
        // an idle desktop is the ordinary case.
        self.needs_render = true;
        Ok(())
    }
}

/// Middle-click paste: a second clipboard, separate from the ordinary one.
impl smithay::wayland::selection::primary_selection::PrimarySelectionHandler
    for ViewportState
{
    fn primary_selection_state(
        &mut self,
    ) -> &mut smithay::wayland::selection::primary_selection::PrimarySelectionState {
        &mut self.primary_selection_state
    }
}

/// Clipboard managers, which need to watch selections they do not own — that
/// is the whole job of one, and the ordinary data-device protocol deliberately
/// does not allow it.
impl smithay::wayland::selection::wlr_data_control::DataControlHandler for ViewportState {
    fn data_control_state(
        &mut self,
    ) -> &mut smithay::wayland::selection::wlr_data_control::DataControlState {
        &mut self.data_control_state
    }
}

/// Something asking the session not to go idle, which is what a video player
/// does while it is playing.
///
/// The surfaces are remembered rather than counted: a client that dies without
/// releasing its inhibitor would otherwise hold the screen awake for ever, and
/// a count cannot tell which one to forget.
impl smithay::wayland::idle_inhibit::IdleInhibitHandler for ViewportState {
    fn inhibit(&mut self, surface: WlSurface) {
        if !self.idle_inhibitors.contains(&surface) {
            self.idle_inhibitors.push(surface);
        }
        self.refresh_idle_inhibit();
    }

    fn uninhibit(&mut self, surface: WlSurface) {
        self.idle_inhibitors.retain(|held| held != &surface);
        self.refresh_idle_inhibit();
    }
}

/// Clients that want to know the session went idle rather than asking the
/// compositor to do something about it — a chat program marking you away.
impl smithay::wayland::idle_notify::IdleNotifierHandler for ViewportState {
    fn idle_notifier_state(&mut self) -> &mut smithay::wayland::idle_notify::IdleNotifierState<Self> {
        &mut self.idle_notifier_state
    }
}

/// Fractional scaling. Nothing to decide: a client asking for it is told the
/// scale of the output it is on, and the render path already works in
/// fractional coordinates.
impl smithay::wayland::fractional_scale::FractionalScaleHandler for ViewportState {
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        use smithay::wayland::compositor::with_states;
        use smithay::wayland::fractional_scale::with_fractional_scale;

        let scale = self
            .space
            .outputs()
            .next()
            .map(|output| output.current_scale().fractional_scale())
            .unwrap_or(1.0);
        with_states(&surface, |states| {
            with_fractional_scale(states, |fractional| {
                fractional.set_preferred_scale(scale);
            });
        });
    }
}

/// The window list, for anything outside the compositor.
///
/// The shell already knows every window — it is drawing them — but nothing
/// outside does, and that is what a taskbar or an alt-tab replacement written
/// as an ordinary client needs.
impl smithay::wayland::foreign_toplevel_list::ForeignToplevelListHandler for ViewportState {
    fn foreign_toplevel_list_state(
        &mut self,
    ) -> &mut smithay::wayland::foreign_toplevel_list::ForeignToplevelListState {
        &mut self.foreign_toplevel_state
    }
}

/// Pointer capture: a game asking for the cursor to stop moving.
///
/// Activated the moment it is created if the pointer is already over the
/// surface, because that is when a game asks — mid-frame, with the cursor
/// where the click was.
impl smithay::wayland::pointer_constraints::PointerConstraintsHandler for ViewportState {
    fn new_constraint(
        &mut self,
        surface: &WlSurface,
        pointer: &smithay::input::pointer::PointerHandle<Self>,
    ) {
        use smithay::wayland::pointer_constraints::with_pointer_constraint;

        let over = self
            .surface_under(pointer.current_location())
            .map(|(under, _)| &under == surface)
            .unwrap_or(false);
        if !over {
            return;
        }
        with_pointer_constraint(surface, pointer, |constraint| {
            if let Some(constraint) = constraint {
                constraint.activate();
            }
        });
    }

    fn remove_constraint(
        &mut self,
        _surface: &WlSurface,
        _pointer: &smithay::input::pointer::PointerHandle<Self>,
    ) {
        // Nothing to undo: the cursor was never moved while locked, so it is
        // already where the client left it.
    }

    fn cursor_position_hint(
        &mut self,
        surface: &WlSurface,
        pointer: &smithay::input::pointer::PointerHandle<Self>,
        location: smithay::utils::Point<f64, smithay::utils::Logical>,
    ) {
        // Where the cursor should reappear when the grab ends. A game usually
        // wants it back under the crosshair rather than wherever it happened
        // to be when the lock started (`src/pointer.c:104`).
        use smithay::wayland::seat::WaylandFocus as _;
        let Some(origin) = self
            .space
            .elements()
            .find(|window| {
                window
                    .wl_surface()
                    .map(|s| &*s == surface)
                    .unwrap_or(false)
            })
            .and_then(|window| self.space.element_geometry(window))
            .map(|geometry| geometry.loc)
        else {
            return;
        };
        let at = location + origin.to_f64();
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        let under = self.surface_under(at);
        pointer.motion(
            self,
            under,
            &smithay::input::pointer::MotionEvent {
                location: at,
                serial,
                time: 0,
            },
        );
        self.needs_render = true;
    }
}

/// linux-dmabuf: how a client that renders on the GPU hands over its frames.
///
/// Without the global there is no way to present a GPU buffer at all. A Vulkan
/// client does not fall back to shared memory — its WSI has nothing to attach
/// to, and rio dies inside
/// vkGetPhysicalDeviceSurfaceCapabilitiesKHR with ERROR_SURFACE_LOST_KHR
/// rather than saying what is missing. Every hardware-accelerated client is in
/// the same position, so this is not one application's requirement.
impl smithay::wayland::dmabuf::DmabufHandler for ViewportState {
    fn dmabuf_state(&mut self) -> &mut smithay::wayland::dmabuf::DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &smithay::wayland::dmabuf::DmabufGlobal,
        dmabuf: smithay::backend::allocator::dmabuf::Dmabuf,
        notifier: smithay::wayland::dmabuf::ImportNotifier,
    ) {
        use smithay::backend::renderer::ImportDma as _;

        // Imported now rather than at first use, because the answer the client
        // is waiting for is whether this buffer is usable at all — and a
        // failure discovered mid-frame has nowhere to go.
        let imported = self
            .udev
            .as_mut()
            .map(|udev| udev.renderer.import_dmabuf(&dmabuf, None).is_ok());
        match imported {
            Some(true) | None => {
                let _ = notifier.successful::<ViewportState>();
            }
            Some(false) => notifier.failed(),
        }
    }
}

/// xdg-activation: "focus this, a token says it was asked for".
///
/// A launcher asks for a token before it starts a program, hands it over in the
/// environment, and the program presents it when its window appears — which is
/// how the window opens focused rather than behind whatever the user moved on
/// to. Without the global at all, wmenu aborts on an assertion before it draws.
///
/// The token is validated by smithay; all that is left is to honour it, and
/// only for a window the shell has been told about (`src/xdg_shell.c:546`).
impl smithay::wayland::xdg_activation::XdgActivationHandler for ViewportState {
    fn activation_state(&mut self) -> &mut smithay::wayland::xdg_activation::XdgActivationState {
        &mut self.xdg_activation_state
    }

    fn request_activation(
        &mut self,
        _token: smithay::wayland::xdg_activation::XdgActivationToken,
        _data: smithay::wayland::xdg_activation::XdgActivationTokenData,
        surface: WlSurface,
    ) {
        let Some(view) = self.views.find_by_surface(&surface) else {
            return;
        };
        // A window that has not been announced has no rectangle to be focused
        // into, and the shell would be told to focus something it has never
        // heard of.
        if !view.mapped {
            return;
        }
        let id = view.id;
        crate::apply::focus_view(self, id);
    }
}

crate::delegate_screencopy!(ViewportState);

smithay::delegate_dispatch2!(ViewportState);
