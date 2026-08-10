// SPDX-License-Identifier: GPL-3.0-or-later
//
// ext-transient-seat-v1: asking for a second, throwaway seat for a short
// piece of work.
//
// A remote-desktop or multi-pointer portal asks for a transient seat when it
// needs to inject input it does not want on the real seat — a virtual pointer
// for somebody controlling the machine, for example. The compositor answers
// with either `ready(name)`, meaning a real `wl_seat` has been minted for the
// client, or `denied`, meaning it has not.
//
// This compositor answers `denied`. It has no virtual-input or remote-desktop
// backend: there is nothing that could sink events into a second seat, so a
// `ready` would hand the client a seat that accepts input nobody will ever
// deliver. `denied` is the truthful reply, and it is the protocol's own
// refusal path — a portal that probes binds the manager, gets `denied`, and
// falls back cleanly instead of hanging on a seat that will never answer. This
// mirrors the honest-no-op convention used elsewhere here (export-dmabuf
// cancels permanently, foreign-toplevel ignores maximize/minimize).
//
// What the global buys is the same as export-dmabuf: a client probes for the
// feature and learns, in one round trip, that this desktop does not offer it,
// rather than treating the compositor as not implementing the protocol at all.
//
// This mirrors foreign_toplevel.rs: old-style Dispatch/GlobalDispatch on the
// state, because these are hand-written rather than provided by Smithay.

use smithay::reexports::wayland_protocols::ext::transient_seat::v1::server::{
    ext_transient_seat_manager_v1::{self, ExtTransientSeatManagerV1},
    ext_transient_seat_v1::{self, ExtTransientSeatV1},
};
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New,
};

/// The protocol has only ever been version 1.
const VERSION: u32 = 1;

/// What the compositor has to be able to do for a request to mean anything.
pub trait TransientSeatHandler {
    fn transient_seat_state(&mut self) -> &mut TransientSeatState;
}

/// The global, and the transient seats handed out to clients.
#[derive(Debug, Default)]
pub struct TransientSeatState {
    seats: Vec<ExtTransientSeatV1>,
}

impl TransientSeatState {
    pub fn new<D>(display: &DisplayHandle) -> Self
    where
        D: GlobalDispatch<ExtTransientSeatManagerV1, ()> + 'static,
    {
        display.create_global::<D, ExtTransientSeatManagerV1, _>(VERSION, ());
        Self::default()
    }
}

impl<D> GlobalDispatch<ExtTransientSeatManagerV1, (), D> for TransientSeatState
where
    D: GlobalDispatch<ExtTransientSeatManagerV1, ()>
        + Dispatch<ExtTransientSeatManagerV1, ()>
        + Dispatch<ExtTransientSeatV1, ()>
        + TransientSeatHandler
        + 'static,
{
    fn bind(
        state: &mut D,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ExtTransientSeatManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, D>,
    ) {
        let _manager = data_init.init(resource, ());
        // The manager is kept alive by the dispatch table; there is no per
        // manager bookkeeping this compositor needs.
        let _ = state;
    }
}

impl<D> Dispatch<ExtTransientSeatManagerV1, (), D> for TransientSeatState
where
    D: Dispatch<ExtTransientSeatManagerV1, ()>
        + Dispatch<ExtTransientSeatV1, ()>
        + TransientSeatHandler
        + 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        _manager: &ExtTransientSeatManagerV1,
        request: ext_transient_seat_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            ext_transient_seat_manager_v1::Request::Create { seat } => {
                let seat = data_init.init(seat, ());
                // There is no virtual-input or remote-desktop backend to feed
                // a second seat, so the honest answer is `denied`: the client
                // asked, and this desktop has nothing to hand it. Sending
                // `ready` would promise a seat that will never accept input.
                seat.denied();
                state.transient_seat_state().seats.push(seat);
            }
            ext_transient_seat_manager_v1::Request::Destroy => {
                // The manager is a single global; destroying the client's
                // handle needs no action beyond the implicit destructor.
            }
            _ => {}
        }
    }
}

impl<D> Dispatch<ExtTransientSeatV1, (), D> for TransientSeatState
where
    D: Dispatch<ExtTransientSeatV1, ()> + TransientSeatHandler + 'static,
{
    fn request(
        _state: &mut D,
        _client: &Client,
        _seat: &ExtTransientSeatV1,
        request: ext_transient_seat_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        if matches!(request, ext_transient_seat_v1::Request::Destroy) {
            // The seat was already denied, so destroying it is the normal
            // teardown and needs no action.
        }
    }

    fn destroyed(
        state: &mut D,
        _client_id: smithay::reexports::wayland_server::backend::ClientId,
        seat: &ExtTransientSeatV1,
        _data: &(),
    ) {
        state
            .transient_seat_state()
            .seats
            .retain(|other| other != seat);
    }
}

/// Wire the dispatch into a compositor state.
#[macro_export]
macro_rules! delegate_transient_seat {
    ($ty:ty) => {
        smithay::reexports::wayland_server::delegate_global_dispatch!($ty: [
            smithay::reexports::wayland_protocols::ext::transient_seat::v1::server::ext_transient_seat_manager_v1::ExtTransientSeatManagerV1: ()
        ] => $crate::transient_seat::TransientSeatState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            smithay::reexports::wayland_protocols::ext::transient_seat::v1::server::ext_transient_seat_manager_v1::ExtTransientSeatManagerV1: ()
        ] => $crate::transient_seat::TransientSeatState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            smithay::reexports::wayland_protocols::ext::transient_seat::v1::server::ext_transient_seat_v1::ExtTransientSeatV1: ()
        ] => $crate::transient_seat::TransientSeatState);
    };
}
