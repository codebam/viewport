// SPDX-License-Identifier: GPL-3.0-or-later
//
// Running calloop inside GLib's main loop. Ports src/glib_loop.c.
//
// WebKit needs a GMainContext pumped on the thread that owns the web view, and
// the compositor needs calloop dispatched. One of them has to be the outer
// loop, and it has to be GLib: a GMainContext polls a set of file descriptors
// that changes as sources come and go, so driving it from outside means
// tracking that set. calloop is a single epoll fd, which GLib can watch as one
// source. Inverting it this way is what the C build settled on, for the same
// reason.

use std::ffi::c_void;
use std::os::fd::{AsFd, AsRawFd};
use std::time::Duration;

use smithay::reexports::calloop::EventLoop;

use crate::state::ViewportState;

type GBool = i32;

const G_IO_IN: u32 = 1;
const G_IO_ERR: u32 = 8;
const G_IO_HUP: u32 = 16;

#[repr(C)]
struct GSourceFuncs {
    prepare: Option<unsafe extern "C" fn(*mut c_void, *mut i32) -> GBool>,
    check: Option<unsafe extern "C" fn(*mut c_void) -> GBool>,
    dispatch: Option<
        unsafe extern "C" fn(*mut c_void, Option<unsafe extern "C" fn()>, *mut c_void) -> GBool,
    >,
    finalize: Option<unsafe extern "C" fn(*mut c_void)>,
    closure_callback: *mut c_void,
    closure_marshal: *mut c_void,
}

// SAFETY: a table of function pointers, never mutated after construction.
unsafe impl Sync for GSourceFuncs {}

/// The GSource, with our fields after GLib's opaque header.
///
/// `g_source_new` allocates `struct_size` bytes and uses the first
/// `sizeof(GSource)` for itself, so anything after that is ours. The header is
/// opaque, hence the byte array — its size comes from GLib at runtime rather
/// than being assumed here.
#[repr(C)]
struct WaylandSource {
    // Filled by GLib. Sized at runtime; see SOURCE_SIZE.
    _header: [u8; 0],
}

/// What the source needs to do its job.
struct Bridge {
    event_loop: *mut EventLoop<'static, ViewportState>,
    state: *mut ViewportState,
    fd_tag: *mut c_void,
}

extern "C" {
    fn g_source_new(funcs: *const GSourceFuncs, struct_size: u32) -> *mut c_void;
    fn g_source_add_unix_fd(source: *mut c_void, fd: i32, events: u32) -> *mut c_void;
    fn g_source_query_unix_fd(source: *mut c_void, tag: *mut c_void) -> u32;
    fn g_source_attach(source: *mut c_void, context: *mut c_void) -> u32;
    fn g_source_unref(source: *mut c_void);
    fn g_source_set_callback(
        source: *mut c_void,
        func: Option<unsafe extern "C" fn()>,
        data: *mut c_void,
        notify: Option<unsafe extern "C" fn(*mut c_void)>,
    );
    fn g_main_context_default() -> *mut c_void;
    fn g_main_loop_new(context: *mut c_void, is_running: GBool) -> *mut c_void;
    fn g_main_loop_run(loop_: *mut c_void);
    fn g_main_loop_quit(loop_: *mut c_void);
    fn g_main_loop_unref(loop_: *mut c_void);
}

/// The size GLib's own `GSource` header occupies.
///
/// Not queryable, and part of GLib's ABI: two pointers, two ints, a pointer,
/// three more pointers, and the private pointer. Over-allocating is harmless —
/// GLib only ever touches its own prefix — so this is deliberately generous
/// rather than exact.
const SOURCE_HEADER: u32 = 256;

/// Where our data starts within the allocation.
const BRIDGE_OFFSET: usize = SOURCE_HEADER as usize;

unsafe fn bridge(source: *mut c_void) -> *mut Bridge {
    (source as *mut u8).add(BRIDGE_OFFSET) as *mut Bridge
}

/// Runs before GLib polls.
///
/// Two things happen here and both are about work that would otherwise sit
/// until something unrelated woke the loop.
///
/// The idle queue is drained, because calloop's idles only run inside a
/// dispatch and a dispatch only happens when its epoll fd signals — so an idle
/// queued while nothing else is happening would wait for unrelated input.
///
/// Then pending events go out to the clients, because a reply sitting in an
/// outgoing buffer has not been sent. Both look the same from the outside:
/// "it only appears when you press a key".
unsafe extern "C" fn prepare(source: *mut c_void, timeout: *mut i32) -> GBool {
    let bridge = &*bridge(source);
    if !bridge.event_loop.is_null() && !bridge.state.is_null() {
        // Zero timeout: dispatch whatever is already pending without waiting.
        let event_loop = &mut *bridge.event_loop;
        let state = &mut *bridge.state;
        let _ = event_loop.dispatch(Some(Duration::ZERO), state);

        // Anything that changed needs a frame before we block, for the same
        // reason: vblank drives rendering and vblank stops when nothing is
        // submitted.
        state.render_if_needed();

        // Push pending events out to clients before GLib blocks
        // (`src/glib_loop.c:71`).
        //
        // A reply written into a client's outgoing buffer is not sent until
        // something flushes it, and the only other flush is at the end of a
        // render — which does not happen while the screen is static. So a
        // client that connects to an idle compositor is answered, and then
        // never hears the answer: it blocks on a read that has nothing behind
        // it until unrelated input causes a frame. The first client of a
        // session hits this every time, because it asks for the registry
        // before anything is moving. It looks exactly like a program that
        // takes seconds to start and then appears the moment you press a key.
        let _ = state.display_handle.flush_clients();
    }
    if !timeout.is_null() {
        // Nothing here needs waking on a timer; the fd does that.
        *timeout = -1;
    }
    0
}

unsafe extern "C" fn check(source: *mut c_void) -> GBool {
    let bridge = &*bridge(source);
    let events = g_source_query_unix_fd(source, bridge.fd_tag);
    (events & (G_IO_IN | G_IO_ERR | G_IO_HUP) != 0) as GBool
}

unsafe extern "C" fn dispatch(
    source: *mut c_void,
    _callback: Option<unsafe extern "C" fn()>,
    _user_data: *mut c_void,
) -> GBool {
    let bridge = &*bridge(source);
    if bridge.event_loop.is_null() || bridge.state.is_null() {
        return 1;
    }

    let event_loop = &mut *bridge.event_loop;
    let state = &mut *bridge.state;

    // The fd is already readable, so this drains without blocking.
    if let Err(e) = event_loop.dispatch(Some(Duration::ZERO), state) {
        tracing::error!("calloop dispatch failed: {e}");
    }

    state.render_if_needed();

    // Whatever that produced, out to the clients. GLib may block before
    // prepare runs again, and a reply left in a buffer is a client left
    // waiting.
    let _ = state.display_handle.flush_clients();

    // G_SOURCE_CONTINUE
    1
}

static FUNCS: GSourceFuncs = GSourceFuncs {
    prepare: Some(prepare),
    check: Some(check),
    dispatch: Some(dispatch),
    finalize: None,
    closure_callback: std::ptr::null_mut(),
    closure_marshal: std::ptr::null_mut(),
};

/// GLib's main loop, with calloop nested inside it.
pub struct GlibLoop {
    main_loop: *mut c_void,
    source: *mut c_void,
}

impl GlibLoop {
    /// Attach `event_loop` to GLib's default context.
    ///
    /// The pointers are held for as long as the loop runs, so both must
    /// outlive it — which is why [`GlibLoop::run`] takes them again rather
    /// than this storing owned values.
    pub fn new(event_loop: &EventLoop<'static, ViewportState>) -> anyhow::Result<Self> {
        // SAFETY: the source is allocated by GLib to our requested size, and
        // the region past its header is ours to write.
        unsafe {
            let source = g_source_new(
                &FUNCS,
                SOURCE_HEADER + std::mem::size_of::<Bridge>() as u32,
            );
            if source.is_null() {
                return Err(anyhow::anyhow!("g_source_new returned NULL"));
            }

            let fd = event_loop.as_fd().as_raw_fd();
            let tag = g_source_add_unix_fd(source, fd, G_IO_IN | G_IO_ERR | G_IO_HUP);

            std::ptr::write(
                bridge(source),
                Bridge {
                    event_loop: std::ptr::null_mut(),
                    state: std::ptr::null_mut(),
                    fd_tag: tag,
                },
            );

            // No callback: prepare/check/dispatch do everything, and GLib
            // refuses to dispatch a source with neither.
            g_source_set_callback(source, None, std::ptr::null_mut(), None);
            g_source_attach(source, g_main_context_default());

            let main_loop = g_main_loop_new(g_main_context_default(), 0);
            Ok(Self { main_loop, source })
        }
    }

    /// Run until [`GlibLoop::quit`] is called.
    ///
    /// Borrows both for the duration, which is what makes storing the raw
    /// pointers sound: nothing else can touch them while this runs.
    pub fn run(
        &mut self,
        event_loop: &mut EventLoop<'static, ViewportState>,
        state: &mut ViewportState,
    ) {
        // SAFETY: both outlive the call, and the source is only dispatched
        // from inside it.
        unsafe {
            let bridge = bridge(self.source);
            (*bridge).event_loop = event_loop as *mut _;
            (*bridge).state = state as *mut _;

            g_main_loop_run(self.main_loop);

            // Cleared so a dispatch after this cannot follow a dangling
            // pointer.
            (*bridge).event_loop = std::ptr::null_mut();
            (*bridge).state = std::ptr::null_mut();
        }
    }

    /// A handle that stops the loop. Cloneable and cheap.
    pub fn signal(&self) -> GlibSignal {
        GlibSignal(self.main_loop)
    }
}

impl Drop for GlibLoop {
    fn drop(&mut self) {
        // SAFETY: both were created here and are released once.
        unsafe {
            g_source_unref(self.source);
            g_main_loop_unref(self.main_loop);
        }
    }
}

/// Stops the GLib loop from anywhere on its own thread.
#[derive(Clone, Copy)]
pub struct GlibSignal(*mut c_void);

impl GlibSignal {
    pub fn quit(&self) {
        // SAFETY: the loop outlives every signal, because the signal is only
        // handed out through the loop.
        unsafe { g_main_loop_quit(self.0) };
    }
}

impl std::fmt::Debug for GlibSignal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GlibSignal").finish_non_exhaustive()
    }
}
