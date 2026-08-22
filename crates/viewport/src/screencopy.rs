// SPDX-License-Identifier: GPL-3.0-or-later
//
// wlr-screencopy-v1: screenshots and screen recording.
//
// Smithay implements this one nowhere, so the dispatch is here. The bindings
// come from wayland-protocols-wlr, which Smithay re-exports, so what is
// written by hand is the protocol's behaviour rather than its wire format.
//
// The copy itself is the renderer's `ExportMem`, which already exists — it is
// what the diagnostic capture in `dump.rs` uses. A frame is composited into an
// offscreen and read back into the client's shared memory, which is the only
// way to hand a client pixels it can use: a client asking for a screenshot
// cannot be given a DMA-BUF it has no way to map.
//
// What is deliberately not here: no permission check. This compositor has no
// notion of a privileged client, and a check that every client passes is worse
// than none because it reads as though it were doing something.

use std::sync::Mutex;

use smithay::output::Output;
use smithay::reexports::wayland_protocols_wlr::screencopy::v1::server::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::{self, ZwlrScreencopyManagerV1},
};
use smithay::reexports::wayland_server::protocol::wl_shm;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};
use smithay::utils::{Physical, Rectangle, Size};

/// What a client asked for.
#[derive(Debug)]
pub struct FrameState {
    pub output: Output,
    /// The region of the output, in its own pixels.
    pub region: Rectangle<i32, Physical>,
    /// Whether to draw the pointer into the copy.
    pub overlay_cursor: bool,
    /// Set once the client has attached a buffer and asked for the copy, so a
    /// second request on the same frame can be refused rather than served.
    pub copied: Mutex<bool>,
}

/// The global.
#[derive(Debug, Default)]
pub struct ScreencopyState;

impl ScreencopyState {
    pub fn new<D>(display: &DisplayHandle) -> Self
    where
        D: GlobalDispatch<ZwlrScreencopyManagerV1, ()> + 'static,
    {
        display.create_global::<D, ZwlrScreencopyManagerV1, _>(3, ());
        Self
    }
}

/// What the compositor has to be able to do for a copy to happen.
pub trait ScreencopyHandler {
    /// Remember that this frame wants a copy of `state.output`.
    ///
    /// The copy itself happens the next time that output is drawn, because
    /// that is where the renderer is: the nested backend's lives inside its
    /// event loop and the compositor cannot reach it from here. Compositing
    /// on the next frame is also what the protocol describes — a client asks
    /// for a frame and is told when one is ready.
    fn queue_copy(
        &mut self,
        frame: &ZwlrScreencopyFrameV1,
        state: &FrameState,
        buffer: &smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer,
        with_damage: bool,
    ) -> Result<(), String>;
}

impl<D> GlobalDispatch<ZwlrScreencopyManagerV1, (), D> for ScreencopyState
where
    D: GlobalDispatch<ZwlrScreencopyManagerV1, ()>
        + Dispatch<ZwlrScreencopyManagerV1, ()>
        + Dispatch<ZwlrScreencopyFrameV1, FrameState>
        + ScreencopyHandler
        + 'static,
{
    fn bind(
        _state: &mut D,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrScreencopyManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, D>,
    ) {
        data_init.init(resource, ());
    }
}

impl<D> Dispatch<ZwlrScreencopyManagerV1, (), D> for ScreencopyState
where
    D: Dispatch<ZwlrScreencopyManagerV1, ()>
        + Dispatch<ZwlrScreencopyFrameV1, FrameState>
        + ScreencopyHandler
        + 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        _manager: &ZwlrScreencopyManagerV1,
        request: zwlr_screencopy_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        let (frame, output, region, overlay_cursor) = match request {
            zwlr_screencopy_manager_v1::Request::CaptureOutput {
                frame,
                overlay_cursor,
                output,
            } => {
                let Some(output) = Output::from_resource(&output) else {
                    // An output that has gone since the client looked it up.
                    // Initialising and failing is the only way to tell it:
                    // there is no error on the manager for this. The output is
                    // a placeholder for the same reason gamma's is — the frame
                    // needs one to exist, and nothing will ever be copied into
                    // it, because `copied` starts set and a second request on
                    // the frame is refused.
                    let frame = data_init.init(
                        frame,
                        FrameState {
                            output: Output::new(
                                "gone".to_owned(),
                                smithay::output::PhysicalProperties {
                                    size: (0, 0).into(),
                                    subpixel: smithay::output::Subpixel::Unknown,
                                    make: String::new(),
                                    model: String::new(),
                                    serial_number: String::new(),
                                },
                            ),
                            region: Rectangle::default(),
                            overlay_cursor: false,
                            copied: Mutex::new(true),
                        },
                    );
                    frame.failed();
                    return;
                };
                let size = output
                    .current_mode()
                    .map(|mode| output.current_transform().transform_size(mode.size))
                    .unwrap_or_default();
                (
                    frame,
                    output,
                    Rectangle::from_size((size.w, size.h).into()),
                    overlay_cursor != 0,
                )
            }
            zwlr_screencopy_manager_v1::Request::CaptureOutputRegion {
                frame,
                overlay_cursor,
                output,
                x,
                y,
                width,
                height,
            } => {
                let Some(output) = Output::from_resource(&output) else {
                    return;
                };
                // The same transformed size `CaptureOutput` uses: the client
                // speaks in output pixels, which after a rotation are not the
                // mode's.
                let size = output
                    .current_mode()
                    .map(|mode| output.current_transform().transform_size(mode.size))
                    .unwrap_or_default();
                let region = clamp_region(x, y, width, height, size);
                if region.size.w == 0 || region.size.h == 0 {
                    // Nothing of the request lies on the output — negatives,
                    // an origin past the far edge, or a zero size. The frame
                    // is initialised and failed rather than dropped, the same
                    // discipline as a capture of an output that has gone: the
                    // object exists so the failure can be named on it, and
                    // `copied` starts set so nothing can be served from it.
                    let frame = data_init.init(
                        frame,
                        FrameState {
                            output,
                            region,
                            overlay_cursor: overlay_cursor != 0,
                            copied: Mutex::new(true),
                        },
                    );
                    frame.failed();
                    return;
                }
                (frame, output, region, overlay_cursor != 0)
            }
            zwlr_screencopy_manager_v1::Request::Destroy => return,
            _ => return,
        };

        let _ = state;
        let size = region.size;
        let frame = data_init.init(
            frame,
            FrameState {
                output,
                region,
                overlay_cursor,
                copied: Mutex::new(false),
            },
        );

        // Only shared memory. A client asking for a screenshot has to be able
        // to read the pixels, and it cannot map a DMA-BUF it did not allocate.
        //
        // XRGB rather than ARGB: a screenshot has no transparency to carry,
        // and a client that treats the fourth byte as alpha would show the
        // whole image as see-through.
        frame.buffer(
            wl_shm::Format::Xrgb8888,
            size.w as u32,
            size.h as u32,
            size.w as u32 * 4,
        );
        // buffer_done arrived in version 3. Sending it to a client that bound
        // an earlier one is a protocol error on an object that has no such
        // event, and the client drops the connection rather than the message.
        if frame.version() >= 3 {
            frame.buffer_done();
        }
    }
}

impl<D> Dispatch<ZwlrScreencopyFrameV1, FrameState, D> for ScreencopyState
where
    D: Dispatch<ZwlrScreencopyFrameV1, FrameState> + ScreencopyHandler + 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        frame: &ZwlrScreencopyFrameV1,
        request: zwlr_screencopy_frame_v1::Request,
        data: &FrameState,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        let (buffer, with_damage) = match request {
            zwlr_screencopy_frame_v1::Request::Copy { buffer } => (buffer, false),
            zwlr_screencopy_frame_v1::Request::CopyWithDamage { buffer } => (buffer, true),
            zwlr_screencopy_frame_v1::Request::Destroy => return,
            _ => return,
        };

        // One copy per frame object. A second is a protocol error rather than
        // a second screenshot.
        {
            let mut copied = data.copied.lock().unwrap();
            if *copied {
                frame.post_error(
                    zwlr_screencopy_frame_v1::Error::AlreadyUsed,
                    "this frame has already been copied",
                );
                return;
            }
            *copied = true;
        }

        if let Err(e) = state.queue_copy(frame, data, &buffer, with_damage) {
            tracing::warn!("screencopy failed: {e}");
            frame.failed();
        }
        // Nothing else is sent here. `flags`, `damage` and `ready` belong to
        // the copy, and the copy has not happened yet — a `ready` now would
        // tell the client its screenshot was in a buffer that still holds
        // whatever it held before.
    }
}

/// Finish a frame whose copy succeeded.
///
/// `with_damage` is whether the client used `copy_with_damage`; a client that
/// used plain `copy` is not expecting a damage event and wlroots does not send
/// one.
pub fn finish(frame: &ZwlrScreencopyFrameV1, region: Rectangle<i32, Physical>, with_damage: bool) {
    // No y_invert: what is read back is already the way round the client
    // expects, and claiming otherwise would have it flip a correct image.
    frame.flags(zwlr_screencopy_frame_v1::Flags::empty());

    // The whole region, because this compositor composites the copy fresh
    // rather than tracking what changed since a previous one — claiming
    // narrower damage would be a lie a recorder acts on. Damage arrived in
    // version 2.
    if with_damage && frame.version() >= 2 {
        frame.damage(0, 0, region.size.w as u32, region.size.h as u32);
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    frame.ready(
        (secs >> 32) as u32,
        (secs & 0xffff_ffff) as u32,
        now.subsec_nanos(),
    );
}

/// The part of the requested rectangle that lies on an output of `size`.
///
/// The client's numbers are unvalidated wire values: negatives, an origin
/// past the far edge, and `i32::MAX` extents are all legal to send. This is
/// their intersection with the output, computed wide rather than through
/// `Rectangle::intersection`, which adds size to location in `i32` and so
/// overflows on exactly the extreme values a client is free to send. An
/// empty result means nothing of the request is on the output, and the frame
/// fails rather than being served with a nonsense region — which is what a
/// negative width used to reach, wrapping the stride event into a value no
/// buffer could match.
fn clamp_region(
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    size: Size<i32, Physical>,
) -> Rectangle<i32, Physical> {
    let x1 = (x as i64).max(0);
    let y1 = (y as i64).max(0);
    let x2 = (x as i64).saturating_add(width as i64).min(size.w as i64);
    let y2 = (y as i64).saturating_add(height as i64).min(size.h as i64);
    // Both differences are checked before they become sizes, because smithay
    // refuses to build a negative one — and "nothing of the request lies on
    // the output" is this function's answer for exactly those.
    let width = x2 - x1;
    let height = y2 - y1;
    if width <= 0 || height <= 0 {
        return Rectangle::default();
    }
    // Bounded by the output's own size, so these always fit back into `i32`.
    Rectangle::new(
        (x1 as i32, y1 as i32).into(),
        (width as u32 as i32, height as u32 as i32).into(),
    )
}

/// Wire the dispatch into a compositor state.
#[macro_export]
macro_rules! delegate_screencopy {
    ($ty:ty) => {
        smithay::reexports::wayland_server::delegate_global_dispatch!($ty: [
            smithay::reexports::wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1: ()
        ] => $crate::screencopy::ScreencopyState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            smithay::reexports::wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1: ()
        ] => $crate::screencopy::ScreencopyState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            smithay::reexports::wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1: $crate::screencopy::FrameState
        ] => $crate::screencopy::ScreencopyState);
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clamp(x: i32, y: i32, width: i32, height: i32) -> Rectangle<i32, Physical> {
        clamp_region(x, y, width, height, Size::from((1920, 1080)))
    }

    #[test]
    fn a_request_inside_the_output_is_untouched() {
        let region = clamp(10, 20, 800, 600);
        assert_eq!(region.loc, (10, 20).into());
        assert_eq!(region.size, (800, 600).into());
    }

    #[test]
    fn the_full_output_is_still_the_full_output() {
        assert_eq!(clamp(0, 0, 1920, 1080).size, (1920, 1080).into());
    }

    #[test]
    fn extents_past_the_far_edge_are_cut_to_it() {
        let region = clamp(1000, 500, 5000, 5000);
        assert_eq!(region.loc, (1000, 500).into());
        assert_eq!(region.size, (920, 580).into());

        // Overhanging on the top left as well: the origin moves in and the
        // size shrinks by as much.
        let region = clamp(-100, -100, 300, 300);
        assert_eq!(region.loc, (0, 0).into());
        assert_eq!(region.size, (200, 200).into());
    }

    #[test]
    fn wire_extremes_never_wrap() {
        // `i32::MAX` everywhere is what a hostile or merely buggy client
        // sends. The old path handed these straight into a rectangle whose
        // stride event wrapped a negative width; these must all stay
        // non-negative and in bounds.
        let region = clamp(i32::MAX, i32::MAX, i32::MAX, i32::MAX);
        assert!(region.is_empty());

        let region = clamp(i32::MIN, i32::MIN, i32::MIN, i32::MIN);
        assert!(region.is_empty());

        // Origin past the far edge with an ordinary extent.
        assert!(clamp(1920, 1080, 100, 100).is_empty());
        assert!(clamp(1919, 1079, 2, 2).size == (1, 1).into());
    }

    #[test]
    fn zero_and_negative_extents_are_empty_not_inverted() {
        assert!(clamp(10, 10, 0, 100).is_empty());
        assert!(clamp(10, 10, 100, 0).is_empty());
        assert!(clamp(10, 10, -5, 100).is_empty());
        assert!(clamp(10, 10, 100, -5).is_empty());
    }

    #[test]
    fn an_output_with_no_mode_clamps_everything_to_empty() {
        let region = clamp_region(0, 0, 100, 100, Size::from((0, 0)));
        assert!(region.is_empty());
    }
}
