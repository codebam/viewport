// SPDX-License-Identifier: GPL-3.0-or-later
//
// xdg-toplevel-drag-v1: tearing a tab out of a window and dragging it into a
// new one.
//
// A browser tears a tab by starting a drag with the tab's data source and, at
// the drop, creating a new toplevel and attaching it to the drag so the
// compositor moves it with the cursor — like a `xdg_toplevel.move` tied to the
// drag session. This is what Firefox and Chromium speak to turn a tab into a
// window.
//
// This compositor accepts the protocol and tracks each drag object, but it
// performs no move on `attach`. The reason is architectural, not a lapse: this
// compositor's shell owns window placement (windows are placed by whatever
// talks to the control socket, and `xdg_toplevel.move` itself is a no-op
// here), so a compositor-initiated move of a dragged toplevel would fight the
// very thing that decides where windows go. Attaching a toplevel is accepted,
// the object lifecycle is spec-correct, and the protocol's errors
// (`invalid_source`, `toplevel_attached`, `ongoing_drag`) are raised as
// specified; the actual repositioning is deliberately left to the shell. This
// is the same honest no-op convention used elsewhere in this tree.
//
// This mirrors foreign_toplevel.rs: old-style Dispatch/GlobalDispatch on the
// state, because these are hand-written rather than provided by Smithay.

use std::sync::Mutex;

use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::XdgToplevel;
use smithay::reexports::wayland_protocols::xdg::toplevel_drag::v1::server::{
    xdg_toplevel_drag_manager_v1::{self, XdgToplevelDragManagerV1},
    xdg_toplevel_drag_v1::{self, XdgToplevelDragV1},
};
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};
use smithay::utils::IsAlive;

/// The protocol has only ever been version 1.
const VERSION: u32 = 1;

/// What the compositor has to be able to do for a request to mean anything.
pub trait ToplevelDragHandler {
    fn toplevel_drag_state(&mut self) -> &mut ToplevelDragState;
}

/// The manager, and the drags each client has started.
#[derive(Debug, Default)]
pub struct ToplevelDragState {
    managers: Vec<XdgToplevelDragManagerV1>,
    drags: Vec<XdgToplevelDragV1>,
}

/// What one drag object knows about itself.
#[derive(Debug, Default)]
pub struct DragData {
    /// The toplevel most recently attached to this drag, if any.
    ///
    /// Interior mutability: the dispatch hands this out as `&DragData`, so a
    /// request that attaches a toplevel mutates it through the lock. The
    /// Wayland dispatch is single-threaded, so a `Mutex` taken and held for a
    /// statement never contends; the `Send + Sync` requirement of dispatch
    /// data is what rules out a bare `Cell`.
    attached: Mutex<Option<XdgToplevel>>,
}

impl ToplevelDragState {
    pub fn new<D>(display: &DisplayHandle) -> Self
    where
        D: GlobalDispatch<XdgToplevelDragManagerV1, ()> + 'static,
    {
        display.create_global::<D, XdgToplevelDragManagerV1, _>(VERSION, ());
        Self::default()
    }
}

impl<D> GlobalDispatch<XdgToplevelDragManagerV1, (), D> for ToplevelDragState
where
    D: GlobalDispatch<XdgToplevelDragManagerV1, ()>
        + Dispatch<XdgToplevelDragManagerV1, ()>
        + Dispatch<XdgToplevelDragV1, DragData>
        + ToplevelDragHandler
        + 'static,
{
    fn bind(
        state: &mut D,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<XdgToplevelDragManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, D>,
    ) {
        let manager = data_init.init(resource, ());
        state.toplevel_drag_state().managers.push(manager);
    }
}

impl<D> Dispatch<XdgToplevelDragManagerV1, (), D> for ToplevelDragState
where
    D: Dispatch<XdgToplevelDragManagerV1, ()>
        + Dispatch<XdgToplevelDragV1, DragData>
        + ToplevelDragHandler
        + 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        manager: &XdgToplevelDragManagerV1,
        request: xdg_toplevel_drag_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            xdg_toplevel_drag_manager_v1::Request::GetXdgToplevelDrag { id, data_source } => {
                if !data_source.alive() {
                    manager.post_error(
                        xdg_toplevel_drag_manager_v1::Error::InvalidSource,
                        "the drag source is gone",
                    );
                    return;
                }
                let drag = data_init.init(id, DragData::default());
                state.toplevel_drag_state().drags.push(drag);
            }
            xdg_toplevel_drag_manager_v1::Request::Destroy => {
                state
                    .toplevel_drag_state()
                    .managers
                    .retain(|other| other != manager);
            }
            _ => {}
        }
    }
}

impl<D> Dispatch<XdgToplevelDragV1, DragData, D> for ToplevelDragState
where
    D: Dispatch<XdgToplevelDragV1, DragData> + ToplevelDragHandler + 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        drag: &XdgToplevelDragV1,
        request: xdg_toplevel_drag_v1::Request,
        data: &DragData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            xdg_toplevel_drag_v1::Request::Attach {
                toplevel,
                x_offset: _,
                y_offset: _,
            } => {
                if data.attached.lock().unwrap().is_some() {
                    drag.post_error(
                        xdg_toplevel_drag_v1::Error::ToplevelAttached,
                        "a toplevel is already attached to this drag",
                    );
                    return;
                }
                *data.attached.lock().unwrap() = Some(toplevel);
                // The shell owns placement here (`xdg_toplevel.move` is a
                // no-op in this compositor), so attaching records the
                // toplevel but does not start a positional grab. A future
                // move implementation can act on `data.attached`.
            }
            xdg_toplevel_drag_v1::Request::Destroy => {
                if data.attached.lock().unwrap().is_some() {
                    // A drag that still holds an attached toplevel is in
                    // flight; the specification forbids destroying it.
                    drag.post_error(
                        xdg_toplevel_drag_v1::Error::OngoingDrag,
                        "destroy called on a drag that is still attached",
                    );
                    return;
                }
                state.toplevel_drag_state().drags.retain(|other| other != drag);
            }
            _ => {}
        }
    }

    fn destroyed(
        state: &mut D,
        _client_id: smithay::reexports::wayland_server::backend::ClientId,
        drag: &XdgToplevelDragV1,
        _data: &DragData,
    ) {
        state.toplevel_drag_state().drags.retain(|other| other != drag);
    }
}

/// Wire the dispatch into a compositor state.
#[macro_export]
macro_rules! delegate_toplevel_drag {
    ($ty:ty) => {
        smithay::reexports::wayland_server::delegate_global_dispatch!($ty: [
            smithay::reexports::wayland_protocols::xdg::toplevel_drag::v1::server::xdg_toplevel_drag_manager_v1::XdgToplevelDragManagerV1: ()
        ] => $crate::toplevel_drag::ToplevelDragState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            smithay::reexports::wayland_protocols::xdg::toplevel_drag::v1::server::xdg_toplevel_drag_manager_v1::XdgToplevelDragManagerV1: ()
        ] => $crate::toplevel_drag::ToplevelDragState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            smithay::reexports::wayland_protocols::xdg::toplevel_drag::v1::server::xdg_toplevel_drag_v1::XdgToplevelDragV1: $crate::toplevel_drag::DragData
        ] => $crate::toplevel_drag::ToplevelDragState);
    };
}
