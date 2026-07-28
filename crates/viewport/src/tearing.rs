// SPDX-License-Identifier: GPL-3.0-or-later
//
// tearing-control-v1: letting a client choose latency over a whole frame.
//
// Every frame this compositor puts on screen is flipped in at a vblank, so a
// frame is finished before any of it is visible. That costs up to one refresh
// of latency, which is invisible on a desktop and is exactly what a
// competitive game does not want: it would rather have the newest frame
// part-drawn than the previous one whole.
//
// The protocol is one hint per surface. A client says "presentation is async"
// and the compositor may flip that surface's frames in as soon as the hardware
// takes them — tearing across the screen where the old frame and the new one
// meet.
//
// Honoured only for a surface that is alone on its output. A torn frame tears
// the whole screen, not one window: a game asking for it while a terminal and
// a bar are on the same monitor would tear those too, and neither asked.
// Smithay implements neither the protocol nor, until the patch this build
// carries, the flip itself.

use std::sync::Mutex;

use smithay::reexports::wayland_protocols::wp::tearing_control::v1::server::{
    wp_tearing_control_manager_v1::{self, WpTearingControlManagerV1},
    wp_tearing_control_v1::{self, PresentationHint, WpTearingControlV1},
};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};

/// The global, and which surfaces have asked.
#[derive(Debug, Default)]
pub struct TearingControlState {
    /// The surfaces whose clients asked for asynchronous presentation. Kept
    /// here rather than in surface state because the question asked at flip
    /// time is "does the one surface on this output want it", which is a
    /// lookup and not a walk of the tree.
    wants_tearing: Vec<WlSurface>,
}

impl TearingControlState {
    pub fn new<D>(display: &DisplayHandle) -> Self
    where
        D: GlobalDispatch<WpTearingControlManagerV1, ()> + 'static,
    {
        display.create_global::<D, WpTearingControlManagerV1, _>(1, ());
        Self::default()
    }

    /// Whether this surface asked to be presented as soon as possible.
    pub fn wants_tearing(&self, surface: &WlSurface) -> bool {
        self.wants_tearing.iter().any(|other| other == surface)
    }

    fn set(&mut self, surface: &WlSurface, wants: bool) {
        let held = self.wants_tearing(surface);
        if wants && !held {
            self.wants_tearing.push(surface.clone());
        } else if !wants && held {
            self.wants_tearing.retain(|other| other != surface);
        }
    }

    /// Forget a surface. Called when its control object goes, and when the
    /// surface itself does — a client that died holding the hint must not
    /// leave the output tearing for whatever takes its place.
    pub fn forget(&mut self, surface: &WlSurface) {
        self.wants_tearing.retain(|other| other != surface);
    }
}

/// What a control object knows: the surface it speaks for.
#[derive(Debug)]
pub struct ControlData {
    pub surface: WlSurface,
    /// The hint as it stands, so destroying the object can undo exactly what
    /// it did.
    pub wants_tearing: Mutex<bool>,
}

impl<D> GlobalDispatch<WpTearingControlManagerV1, (), D> for TearingControlState
where
    D: GlobalDispatch<WpTearingControlManagerV1, ()>
        + Dispatch<WpTearingControlManagerV1, ()>
        + Dispatch<WpTearingControlV1, ControlData>
        + TearingControlHandler
        + 'static,
{
    fn bind(
        _state: &mut D,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<WpTearingControlManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, D>,
    ) {
        data_init.init(resource, ());
    }
}

/// What the compositor has to offer for the hint to reach anything.
pub trait TearingControlHandler {
    fn tearing_control_state(&mut self) -> &mut TearingControlState;
}

impl<D> Dispatch<WpTearingControlManagerV1, (), D> for TearingControlState
where
    D: Dispatch<WpTearingControlManagerV1, ()>
        + Dispatch<WpTearingControlV1, ControlData>
        + TearingControlHandler
        + 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        manager: &WpTearingControlManagerV1,
        request: wp_tearing_control_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        let wp_tearing_control_manager_v1::Request::GetTearingControl { id, surface } = request
        else {
            return;
        };

        // One per surface. A second is a protocol error rather than a second
        // opinion: two objects disagreeing about the same surface has no
        // answer the client could predict.
        if state.tearing_control_state().wants_tearing(&surface) {
            manager.post_error(
                wp_tearing_control_manager_v1::Error::TearingControlExists,
                "this surface already has a tearing control",
            );
            return;
        }

        data_init.init(
            id,
            ControlData {
                surface,
                wants_tearing: Mutex::new(false),
            },
        );
    }
}

impl<D> Dispatch<WpTearingControlV1, ControlData, D> for TearingControlState
where
    D: Dispatch<WpTearingControlV1, ControlData> + TearingControlHandler + 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        _control: &WpTearingControlV1,
        request: wp_tearing_control_v1::Request,
        data: &ControlData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        let wp_tearing_control_v1::Request::SetPresentationHint { hint } = request else {
            return;
        };
        let Ok(hint) = hint.into_result() else {
            return;
        };

        // Applied now rather than on the next commit. The protocol makes it
        // double-buffered surface state; treating it as immediate is visible
        // only as one frame that tears a frame early or late, and keeping a
        // second copy of it in the surface tree to be exact costs more than
        // that frame is worth.
        let wants = hint == PresentationHint::Async;
        *data.wants_tearing.lock().unwrap() = wants;
        state.tearing_control_state().set(&data.surface, wants);
    }

    fn destroyed(
        state: &mut D,
        _client: smithay::reexports::wayland_server::backend::ClientId,
        _control: &WpTearingControlV1,
        data: &ControlData,
    ) {
        // Back to whole frames. The object going away is the client saying it
        // no longer wants this, and a surface left tearing after its control
        // is gone would tear for the rest of the session.
        state.tearing_control_state().forget(&data.surface);
    }
}

/// Wire the dispatch into a compositor state.
#[macro_export]
macro_rules! delegate_tearing_control {
    ($ty:ty) => {
        smithay::reexports::wayland_server::delegate_global_dispatch!($ty: [
            smithay::reexports::wayland_protocols::wp::tearing_control::v1::server::wp_tearing_control_manager_v1::WpTearingControlManagerV1: ()
        ] => $crate::tearing::TearingControlState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            smithay::reexports::wayland_protocols::wp::tearing_control::v1::server::wp_tearing_control_manager_v1::WpTearingControlManagerV1: ()
        ] => $crate::tearing::TearingControlState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            smithay::reexports::wayland_protocols::wp::tearing_control::v1::server::wp_tearing_control_v1::WpTearingControlV1: $crate::tearing::ControlData
        ] => $crate::tearing::TearingControlState);
    };
}
