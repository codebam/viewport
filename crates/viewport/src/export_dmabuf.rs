// SPDX-License-Identifier: GPL-3.0-or-later
//
// wlr-export-dmabuf-v1: capturing an output's pixels as DMA-BUFs.
//
// A screen-capture client that binds wlr-screencopy gets a full software read
// path; wlr-export-dmabuf is the zero-copy cousin that hands the client the
// DMA-BUF fds of the scanout planes. This compositor can no-op it the honest
// way: every capture is answered with `cancel(permanent)`. That is legal —
// the protocol explicitly lets a capture fail at any time before `ready` —
// and it is truthful, because there is no dmabuf export path here, so any
// client would get nothing back no matter how often it asked. What the global
// buys is a client that probes for it and falls back gracefully instead of
// treating the compositor as lacking the feature at all.
//
// This mirrors foreign_toplevel.rs: old-style Dispatch/GlobalDispatch on the
// state, because these are hand-written rather than provided by Smithay.

use smithay::reexports::wayland_protocols_wlr::export_dmabuf::v1::server::{
    zwlr_export_dmabuf_frame_v1::{self, CancelReason, ZwlrExportDmabufFrameV1},
    zwlr_export_dmabuf_manager_v1::{self, ZwlrExportDmabufManagerV1},
};
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New,
};

/// The protocol has only ever been version 1.
const VERSION: u32 = 1;

/// What the compositor has to be able to do for a request to mean anything.
pub trait ExportDmabufHandler {
    fn export_dmabuf_state(&mut self) -> &mut ExportDmabufState;
}

/// The global, and the managers each client holds.
#[derive(Debug, Default)]
pub struct ExportDmabufState {
    managers: Vec<ZwlrExportDmabufManagerV1>,
}

impl ExportDmabufState {
    pub fn new<D>(display: &DisplayHandle) -> Self
    where
        D: GlobalDispatch<ZwlrExportDmabufManagerV1, ()> + 'static,
    {
        display.create_global::<D, ZwlrExportDmabufManagerV1, _>(VERSION, ());
        Self::default()
    }
}

impl<D> GlobalDispatch<ZwlrExportDmabufManagerV1, (), D> for ExportDmabufState
where
    D: GlobalDispatch<ZwlrExportDmabufManagerV1, ()>
        + Dispatch<ZwlrExportDmabufManagerV1, ()>
        + Dispatch<ZwlrExportDmabufFrameV1, ()>
        + ExportDmabufHandler
        + 'static,
{
    fn bind(
        state: &mut D,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrExportDmabufManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, D>,
    ) {
        let manager = data_init.init(resource, ());
        state.export_dmabuf_state().managers.push(manager);
    }
}

impl<D> Dispatch<ZwlrExportDmabufManagerV1, (), D> for ExportDmabufState
where
    D: Dispatch<ZwlrExportDmabufManagerV1, ()>
        + Dispatch<ZwlrExportDmabufFrameV1, ()>
        + ExportDmabufHandler
        + 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        manager: &ZwlrExportDmabufManagerV1,
        request: zwlr_export_dmabuf_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            zwlr_export_dmabuf_manager_v1::Request::CaptureOutput { frame, .. } => {
                let frame = data_init.init(frame, ());
                // There is no dmabuf export path here, so the honest answer is
                // immediate, permanent failure — exactly what a compositor
                // without the capability must tell the client. Anything else
                // would leave it waiting for a `ready` that will never come.
                frame.cancel(CancelReason::Permanent);
            }
            zwlr_export_dmabuf_manager_v1::Request::Destroy => {
                state
                    .export_dmabuf_state()
                    .managers
                    .retain(|other| other != manager);
            }
            _ => {}
        }
    }
}

impl<D> Dispatch<ZwlrExportDmabufFrameV1, (), D> for ExportDmabufState
where
    D: Dispatch<ZwlrExportDmabufFrameV1, ()> + ExportDmabufHandler + 'static,
{
    fn request(
        _state: &mut D,
        _client: &Client,
        _frame: &ZwlrExportDmabufFrameV1,
        request: zwlr_export_dmabuf_frame_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        if matches!(
            request,
            zwlr_export_dmabuf_frame_v1::Request::Destroy
        ) {
            // The object has already been cancelled; the client destroying it
            // is the normal teardown and needs no action.
        }
    }
}

/// Wire the dispatch into a compositor state.
#[macro_export]
macro_rules! delegate_export_dmabuf {
    ($ty:ty) => {
        smithay::reexports::wayland_server::delegate_global_dispatch!($ty: [
            smithay::reexports::wayland_protocols_wlr::export_dmabuf::v1::server::zwlr_export_dmabuf_manager_v1::ZwlrExportDmabufManagerV1: ()
        ] => $crate::export_dmabuf::ExportDmabufState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            smithay::reexports::wayland_protocols_wlr::export_dmabuf::v1::server::zwlr_export_dmabuf_manager_v1::ZwlrExportDmabufManagerV1: ()
        ] => $crate::export_dmabuf::ExportDmabufState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            smithay::reexports::wayland_protocols_wlr::export_dmabuf::v1::server::zwlr_export_dmabuf_frame_v1::ZwlrExportDmabufFrameV1: ()
        ] => $crate::export_dmabuf::ExportDmabufState);
    };
}
