// SPDX-License-Identifier: GPL-3.0-or-later
//
// Smithay protocol handler implementations.
//
// Adapted from smithay's `smallvil` example, which is MIT-licensed; this crate
// is GPL-3.0-or-later, which MIT permits.

mod compositor;
mod xdg_shell;

pub mod layer_shell;
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

smithay::delegate_dispatch2!(ViewportState);
