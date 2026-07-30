// SPDX-License-Identifier: MIT
//
// The Rust side of the WPEPlatform shim.
//
// Everything intricate about GObject subclassing lives in shim/viewport-shim.c
// and stops there. What crosses this boundary is a callback and a plain struct
// of dma-buf fds — no GObject, no refcounting, nothing that has to be got
// right twice.

use std::ffi::{c_char, c_void, CStr, CString};
use std::path::Path;

use anyhow::{anyhow, Result};

use crate::{Frame, Plane};

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct ShimFrame {
    width: u32,
    height: u32,
    format: u32,
    modifier: u64,

    n_planes: u32,
    fds: [i32; 4],
    offsets: [u32; 4],
    strides: [u32; 4],

    fence_fd: i32,
    token: *mut c_void,
}

#[repr(C)]
struct ShimCallbacks {
    user: *mut c_void,
    render_frame: Option<unsafe extern "C" fn(*mut c_void, *const ShimFrame) -> bool>,
}

#[repr(C)]
struct ShimDisplayConfig {
    primary_node: *const c_char,
    render_node: *const c_char,
    format_codes: *const u32,
    format_modifiers: *const u64,
    n_formats: u32,
}

enum ShimDisplay {}

extern "C" {
    fn viewport_shim_display_new(
        config: *const ShimDisplayConfig,
        callbacks: *const ShimCallbacks,
        error_out: *mut *mut c_char,
    ) -> *mut ShimDisplay;
    fn viewport_shim_display_free(display: *mut ShimDisplay);
    fn viewport_shim_display_handle(display: *mut ShimDisplay) -> *mut c_void;
    fn viewport_shim_frame_done(display: *mut ShimDisplay, token: *mut c_void);
    fn viewport_shim_display_resize(display: *mut ShimDisplay, width: u32, height: u32);
    fn viewport_shim_display_show(display: *mut ShimDisplay);
    fn viewport_shim_pointer_motion(
        display: *mut ShimDisplay,
        time_msec: u32,
        x: f64,
        y: f64,
        modifiers: u32,
    );
    fn viewport_shim_pointer_button(
        display: *mut ShimDisplay,
        time_msec: u32,
        x: f64,
        y: f64,
        button: u32,
        pressed: bool,
        modifiers: u32,
    );
    fn viewport_shim_pointer_axis(
        display: *mut ShimDisplay,
        time_msec: u32,
        x: f64,
        y: f64,
        dx: f64,
        dy: f64,
        precise: bool,
        modifiers: u32,
    );
    fn viewport_shim_keyboard_key(
        display: *mut ShimDisplay,
        time_msec: u32,
        keycode: u32,
        keysym: u32,
        pressed: bool,
        modifiers: u32,
    );
    fn viewport_shim_string_free(string: *mut c_char);
}

/// What a frame arriving from WebKit is handed to.
///
/// Called on the thread driving the GLib main context, which is the
/// compositor's own thread — so there is no synchronisation here and none is
/// needed.
pub trait FrameSink: Send {
    /// Take a frame. Returning false fails the frame, which WebKit treats as
    /// a rendering error rather than a dropped update.
    fn frame(&mut self, frame: Frame, token: FrameToken) -> bool;
}

/// A frame that has been handed over but not yet presented.
///
/// WebKit will not paint the next frame until this is returned, which is what
/// keeps the shell's paint rate on vblank instead of free-running. Dropping it
/// without acknowledging stalls the engine permanently, so it is not `Drop`:
/// it has to be passed back explicitly.
#[derive(Debug)]
pub struct FrameToken(*mut c_void);

impl FrameToken {
    /// The underlying `WPEBuffer`, for the release call in
    /// [`crate::webkit::WebView::frame_release`].
    pub fn as_ptr(&self) -> *mut c_void {
        self.0
    }
}

// SAFETY: the pointer is an opaque WPEBuffer handle that is only ever passed
// back to the shim on the same thread that produced it.
unsafe impl Send for FrameToken {}

/// A WPE display, subclassed in C, driving one web view.
pub struct Display {
    inner: *mut ShimDisplay,
    // Kept alive for as long as the display can call into it.
    _sink: Box<Box<dyn FrameSink>>,
}

impl Display {
    /// Create the display.
    ///
    /// `render_node` must be the node backing the compositor's renderer.
    /// WebKit allocates on the device it is told about, and a buffer from
    /// another device cannot be imported — it fails at import time, long after
    /// the mistake.
    ///
    /// Both nodes are required. `wpe_drm_device_new` asserts on a NULL primary
    /// node, and a GLib assertion is a warning rather than a failure: the
    /// display comes back with no device at all and fails to connect later,
    /// for a reason that looks nothing like the cause.
    ///
    /// `formats` is what WebKit may choose from. Offering a format the
    /// compositor cannot import produces a shell that never appears.
    pub fn new(
        primary_node: &Path,
        render_node: &Path,
        formats: &[(u32, u64)],
        sink: Box<dyn FrameSink>,
    ) -> Result<Self> {
        let render = CString::new(render_node.as_os_str().as_encoded_bytes())
            .map_err(|_| anyhow!("the render node path contains a nul byte"))?;
        let primary = CString::new(primary_node.as_os_str().as_encoded_bytes())
            .map_err(|_| anyhow!("the primary node path contains a nul byte"))?;

        let codes: Vec<u32> = formats.iter().map(|(code, _)| *code).collect();
        let modifiers: Vec<u64> = formats.iter().map(|(_, modifier)| *modifier).collect();

        // Double-boxed so the trait object is one thin pointer that survives
        // being cast through `void *`.
        let mut sink = Box::new(sink);
        let user = &mut *sink as *mut Box<dyn FrameSink> as *mut c_void;

        let config = ShimDisplayConfig {
            primary_node: primary.as_ptr(),
            render_node: render.as_ptr(),
            format_codes: codes.as_ptr(),
            format_modifiers: modifiers.as_ptr(),
            n_formats: codes.len() as u32,
        };
        let callbacks = ShimCallbacks {
            user,
            render_frame: Some(render_frame),
        };

        let mut error: *mut c_char = std::ptr::null_mut();
        // SAFETY: every pointer in `config` outlives this call, and the shim
        // copies what it needs.
        let inner = unsafe { viewport_shim_display_new(&config, &callbacks, &mut error) };

        if inner.is_null() {
            let message = if error.is_null() {
                "the display could not be created".to_owned()
            } else {
                // SAFETY: the shim promises a nul-terminated string it
                // allocated, freed with its own free function.
                let owned = unsafe { CStr::from_ptr(error) }
                    .to_string_lossy()
                    .into_owned();
                unsafe { viewport_shim_string_free(error) };
                owned
            };
            return Err(anyhow!(message));
        }

        Ok(Self { inner, _sink: sink })
    }

    /// The `WPEDisplay` pointer, for handing to `webkit_web_view_new`.
    pub fn handle(&self) -> *mut c_void {
        // SAFETY: `inner` is non-null for the lifetime of `self`.
        unsafe { viewport_shim_display_handle(self.inner) }
    }

    /// Acknowledge a frame: it reached the screen, so the frame clock may
    /// schedule the next paint.
    ///
    /// Does not give the buffer back — see [`Display::frame_release`]. The
    /// texture sampling it is usually still on screen at this point.
    pub fn frame_done(&self, token: &FrameToken) {
        // SAFETY: the token came from this display's own callback.
        unsafe { viewport_shim_frame_done(self.inner, token.0) };
    }


    pub fn resize(&self, width: u32, height: u32) {
        // SAFETY: as above.
        unsafe { viewport_shim_display_resize(self.inner, width, height) };
    }

    /// Map the view and focus it.
    ///
    /// Required before WebKit paints anything. An unmapped view produces no
    /// frames, which looks exactly like a page that loaded and did nothing.
    pub fn show(&self) {
        // SAFETY: as above.
        unsafe { viewport_shim_display_show(self.inner) };
    }

    /// The pointer moved, in the layout's own coordinates.
    ///
    /// A negative position is a leave — the pointer moved onto a client
    /// window — and has to be sent, or a `:hover` state stays lit under
    /// whatever the pointer went to.
    pub fn pointer_motion(&self, time_msec: u32, x: f64, y: f64, modifiers: u32) {
        // SAFETY: as above.
        unsafe { viewport_shim_pointer_motion(self.inner, time_msec, x, y, modifiers) };
    }

    pub fn pointer_button(
        &self,
        time_msec: u32,
        x: f64,
        y: f64,
        button: u32,
        pressed: bool,
        modifiers: u32,
    ) {
        // SAFETY: as above.
        unsafe {
            viewport_shim_pointer_button(self.inner, time_msec, x, y, button, pressed, modifiers)
        };
    }

    // Eight, because the C entry point in `shim/viewport-shim.c` takes eight.
    // Bundling them into a struct here would mean unpacking it again one line
    // later, and the wrapper is worth more when it reads like the thing it
    // wraps.
    #[allow(clippy::too_many_arguments)]
    pub fn pointer_axis(
        &self,
        time_msec: u32,
        x: f64,
        y: f64,
        dx: f64,
        dy: f64,
        precise: bool,
        modifiers: u32,
    ) {
        // SAFETY: as above.
        unsafe {
            viewport_shim_pointer_axis(self.inner, time_msec, x, y, dx, dy, precise, modifiers)
        };
    }

    pub fn keyboard_key(
        &self,
        time_msec: u32,
        keycode: u32,
        keysym: u32,
        pressed: bool,
        modifiers: u32,
    ) {
        // SAFETY: as above.
        unsafe {
            viewport_shim_keyboard_key(self.inner, time_msec, keycode, keysym, pressed, modifiers)
        };
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        // SAFETY: called once, and nothing uses `inner` afterwards.
        unsafe { viewport_shim_display_free(self.inner) };
    }
}

impl std::fmt::Debug for Display {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Display").finish_non_exhaustive()
    }
}

/// The C callback. Everything unsafe about the boundary is confined here.
unsafe extern "C" fn render_frame(user: *mut c_void, frame: *const ShimFrame) -> bool {
    if user.is_null() || frame.is_null() {
        return false;
    }

    // A panic must not cross back into C, where unwinding is undefined.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let sink = &mut *(user as *mut Box<dyn FrameSink>);
        let raw = &*frame;

        let planes: Vec<Plane> = (0..raw.n_planes.min(4) as usize)
            .filter_map(|i| {
                // The fds are borrowed for the duration of the callback, so
                // anything kept has to be duplicated.
                let borrowed = std::os::fd::BorrowedFd::borrow_raw(raw.fds[i]);
                borrowed.try_clone_to_owned().ok().map(|fd| Plane {
                    fd,
                    offset: raw.offsets[i],
                    stride: raw.strides[i],
                })
            })
            .collect();

        if planes.len() != raw.n_planes.min(4) as usize {
            tracing::error!("could not duplicate every plane fd; dropping the frame");
            return false;
        }

        let fence = if raw.fence_fd >= 0 {
            std::os::fd::BorrowedFd::borrow_raw(raw.fence_fd)
                .try_clone_to_owned()
                .ok()
        } else {
            None
        };

        sink.frame(
            Frame {
                planes,
                format: raw.format,
                modifier: raw.modifier,
                width: raw.width,
                height: raw.height,
                fence,
            },
            FrameToken(raw.token),
        )
    }));

    match result {
        Ok(accepted) => accepted,
        Err(_) => {
            tracing::error!("the frame handler panicked");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Discard;
    impl FrameSink for Discard {
        fn frame(&mut self, _frame: Frame, _token: FrameToken) -> bool {
            true
        }
    }

    #[test]
    fn a_display_needs_a_render_node_that_exists() {
        // The shim only checks that a node was given; WPE checks the rest when
        // it connects. What matters here is that the error comes back as a
        // message rather than a crash.
        // XR24 (XRGB8888), linear. What matters is that the GObject
        // subclasses construct: a wrong vfunc table aborts inside GLib rather
        // than returning an error.
        let result = Display::new(
            Path::new("/dev/dri/card1"),
            Path::new("/dev/dri/renderD128"),
            &[(0x3432_4258, 0)],
            Box::new(Discard),
        );
        match result {
            Ok(display) => {
                assert!(!display.handle().is_null());
            }
            Err(e) => {
                eprintln!("no WPE display here ({e}); skipping");
            }
        }
    }
}
