// SPDX-License-Identifier: GPL-3.0-or-later
//
// The crash surface, probed over the wire.
//
// Both earlier audit passes swept behaviour; neither drove the protocol
// surface with values a hostile or merely buggy client is free to send.
// These tests do exactly that, each against a real headless compositor with
// a real client: the region arithmetic of `zwlr_screencopy_v1` at the i32
// limits, the frame lifecycle through destroy races and a buffer that
// cannot hold what it was asked to, wlr-layer-shell margins and exclusive
// zones at the limits of the wire's `int`, and an `xdg_popup` whose
// positioner and parent are as degenerate as the protocol allows.
//
// Every one of them ends the same two ways: the compositor answers with
// the protocol's own refusal (`failed`, a configure, or a protocol error
// that costs only the offending client its connection), and a fresh
// ordinary request still succeeds afterwards. A panic, an abort or a
// wedge anywhere in those handlers fails the suite here rather than taking
// a session down.

mod common;

use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use common::Compositor;
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_output, wl_registry, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::xdg::shell::client::{
    xdg_popup, xdg_positioner, xdg_surface, xdg_toplevel, xdg_wm_base,
};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1, zwlr_screencopy_manager_v1,
};

/// Dispatch and flush once, answering whether the connection is still
/// usable. The shape `popup_fade`'s pump has, without that suite's window.
fn pump(connection: &Connection, queue: &mut EventQueue<Probe>, probe: &mut Probe) -> bool {
    if let Some(guard) = connection.prepare_read() {
        if let Err(err) = guard.read() {
            let empty = matches!(
                &err,
                wayland_client::backend::WaylandError::Io(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
            );
            if !empty {
                return false;
            }
        }
    }
    if queue.dispatch_pending(probe).is_err() {
        return false;
    }
    connection.flush().is_ok()
}

/// Pump until `done` answers, or `within` passes. False on either.
fn pump_until(
    connection: &Connection,
    queue: &mut EventQueue<Probe>,
    probe: &mut Probe,
    within: Duration,
    done: impl Fn(&Probe) -> bool,
) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if done(probe) {
            return true;
        }
        if !pump(connection, queue, probe) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    done(probe)
}

/// The compositor process is still running, which is the first thing any
/// of these probes wants to know after it has misbehaved at a client.
fn still_running(compositor: &mut Compositor) -> bool {
    compositor
        .child
        .try_wait()
        .expect("the compositor's own state")
        .is_none()
}

/// Connect a client to the compositor's Wayland display, with one
/// roundtrip for the globals.
fn connect(compositor: &Compositor) -> (Connection, EventQueue<Probe>, Probe) {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR");
    let display = compositor
        .wayland_display()
        .expect("the compositor never announced a Wayland display");
    let connection = Connection::from_socket(
        UnixStream::connect(PathBuf::from(runtime).join(display)).expect("Wayland socket"),
    )
    .expect("Wayland connection");
    let mut queue = connection.new_event_queue();
    let qh = queue.handle();
    connection.display().get_registry(&qh, ());
    let mut probe = Probe::default();
    queue.roundtrip(&mut probe).expect("Wayland globals");
    (connection, queue, probe)
}

/// One shm pool of `size` bytes and one buffer in it, the shape every
/// capture-serving test needs. `popup_fade`'s, without the painting.
fn shm_buffer(
    shm: &wl_shm::WlShm,
    width: i32,
    height: i32,
    stride: i32,
    qh: &QueueHandle<Probe>,
) -> (wl_buffer::WlBuffer, OwnedFd) {
    let size = stride as usize * height as usize;
    let fd = unsafe { libc::memfd_create(c"crash-probe".as_ptr(), 0) };
    assert!(fd >= 0, "memfd_create failed");
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    assert_eq!(
        unsafe { libc::ftruncate(fd.as_raw_fd(), size as libc::off_t) },
        0
    );
    let pool = shm.create_pool(
        unsafe { BorrowedFd::borrow_raw(fd.as_raw_fd()) },
        size as i32,
        qh,
        (),
    );
    let buffer = pool.create_buffer(0, width, height, stride, wl_shm::Format::Xrgb8888, qh, ());
    pool.destroy();
    (buffer, fd)
}

/// A capture the client has asked for: whether the frame was answered,
/// and with what.
#[derive(Default, Clone)]
struct CaptureState {
    width: i32,
    height: i32,
    stride: i32,
    answered: bool,
    ready: bool,
    failed: bool,
}

/// The client every probe in this suite drives.
#[derive(Default)]
struct Probe {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    layer_shell: Option<zwlr_layer_shell_v1::ZwlrLayerShellV1>,
    screencopy: Option<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1>,
    outputs: Vec<wl_output::WlOutput>,

    captures: std::collections::HashMap<wayland_client::backend::ObjectId, CaptureState>,

    /// Which configure arrived, by the surface it belongs to.
    configured: std::collections::HashMap<wayland_client::backend::ObjectId, u32>,
    /// The serial the last layer configure carried, by surface.
    layer_serials: std::collections::HashMap<wayland_client::backend::ObjectId, u32>,
    closed: bool,
}

impl Dispatch<wl_registry::WlRegistry, ()> for Probe {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        else {
            return;
        };
        match interface.as_str() {
            "wl_compositor" => state.compositor = Some(registry.bind(name, version.min(6), qh, ())),
            "wl_shm" => state.shm = Some(registry.bind(name, version.min(1), qh, ())),
            "xdg_wm_base" => state.wm_base = Some(registry.bind(name, version.min(6), qh, ())),
            "zwlr_layer_shell_v1" => {
                state.layer_shell = Some(registry.bind(name, version.min(4), qh, ()))
            }
            "zwlr_screencopy_manager_v1" => {
                state.screencopy = Some(registry.bind(name, version.min(3), qh, ()))
            }
            "wl_output" => state
                .outputs
                .push(registry.bind(name, version.min(4), qh, ())),
            _ => {}
        }
    }
}

delegate_noop!(Probe: ignore wl_compositor::WlCompositor);
delegate_noop!(Probe: ignore wl_shm::WlShm);
delegate_noop!(Probe: ignore wl_buffer::WlBuffer);
delegate_noop!(Probe: ignore wl_shm_pool::WlShmPool);
delegate_noop!(Probe: ignore wl_surface::WlSurface);
delegate_noop!(Probe: ignore wl_output::WlOutput);
delegate_noop!(Probe: ignore zwlr_layer_shell_v1::ZwlrLayerShellV1);
delegate_noop!(Probe: ignore zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1);
delegate_noop!(Probe: ignore xdg_positioner::XdgPositioner);
delegate_noop!(Probe: ignore xdg_popup::XdgPopup);

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for Probe {
    fn event(
        _: &mut Self,
        wm_base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for Probe {
    fn event(
        state: &mut Self,
        surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            surface.ack_configure(serial);
            state.configured.insert(surface.id(), serial);
        }
    }
}

delegate_noop!(Probe: ignore xdg_toplevel::XdgToplevel);

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for Probe {
    fn event(
        state: &mut Self,
        surface: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure { serial, .. } => {
                surface.ack_configure(serial);
                state.layer_serials.insert(surface.id(), serial);
            }
            zwlr_layer_surface_v1::Event::Closed => state.closed = true,
            _ => {}
        }
    }
}

impl Dispatch<zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1, ()> for Probe {
    fn event(
        state: &mut Self,
        frame: &zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let Some(capture) = state.captures.get_mut(&frame.id()) else {
            return;
        };
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                width,
                height,
                stride,
                ..
            } => {
                capture.width = width as i32;
                capture.height = height as i32;
                capture.stride = stride as i32;
            }
            zwlr_screencopy_frame_v1::Event::BufferDone => capture.answered = true,
            zwlr_screencopy_frame_v1::Event::Ready { .. } => {
                capture.ready = true;
                capture.answered = true;
            }
            zwlr_screencopy_frame_v1::Event::Failed => {
                capture.failed = true;
                capture.answered = true;
            }
            _ => {
                let _ = qh;
            }
        }
    }
}

impl Probe {
    /// The first output, which is the one every capture here names.
    fn first_output(&self) -> wl_output::WlOutput {
        self.outputs.first().cloned().expect("an output to capture")
    }

    /// Ask for a capture of a region, and remember the frame it made.
    fn capture_region(
        &mut self,
        qh: &QueueHandle<Self>,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1 {
        let manager = self.screencopy.as_ref().expect("screencopy manager");
        let frame =
            manager.capture_output_region(0, &self.first_output(), x, y, width, height, qh, ());
        self.captures.insert(frame.id(), CaptureState::default());
        frame
    }

    /// Ask for a capture of the whole output.
    fn capture_output(
        &mut self,
        qh: &QueueHandle<Self>,
    ) -> zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1 {
        let manager = self.screencopy.as_ref().expect("screencopy manager");
        let frame = manager.capture_output(0, &self.first_output(), qh, ());
        self.captures.insert(frame.id(), CaptureState::default());
        frame
    }

    fn answered(&self) -> bool {
        self.captures.values().all(|capture| capture.answered)
    }
}

/// The wire's own extremes, and the ordinary shapes near them: every one of
/// these is legal for a client to send, and none may cost more than a
/// `failed` (or a clamped, still-served region).
const HOSTILE_REGIONS: &[(i32, i32, i32, i32)] = &[
    (i32::MIN, i32::MIN, i32::MAX, i32::MAX),
    (i32::MAX, i32::MAX, i32::MIN, i32::MIN),
    (i32::MAX, 0, i32::MAX, 10),
    (i32::MIN, 0, i32::MAX, 10),
    (10, 10, 0, 100),
    (10, 10, 100, 0),
    (10, 10, -5, 100),
    (10, 10, 100, -5),
    (1920, 1080, 100, 100),
    (-100, -100, 5000, 5000),
    (0, 0, 1, 1),
];

#[test]
fn screencopy_region_extremes_are_answered_not_fatal() {
    let mut compositor = Compositor::start("sc-extremes");
    let (connection, mut queue, mut probe) = connect(&compositor);
    let qh = queue.handle();

    for &(x, y, width, height) in HOSTILE_REGIONS {
        probe.capture_region(&qh, x, y, width, height);
    }
    assert!(
        pump_until(
            &connection,
            &mut queue,
            &mut probe,
            Duration::from_secs(10),
            |probe| { probe.answered() }
        ),
        "a hostile region was never answered:\n{}",
        compositor.log()
    );
    // Every frame was told something: a `failed` for the regions with
    // nothing on the output, a buffer for the ones that clamped onto it.
    for capture in probe.captures.values() {
        assert!(
            capture.failed || (capture.width > 0 && capture.stride > 0),
            "a hostile region was answered with neither a failure nor a buffer"
        );
    }
    assert!(still_running(&mut compositor));

    // And the session still serves an ordinary capture afterwards.
    probe.captures.clear();
    let frame = probe.capture_output(&qh);
    assert!(
        pump_until(
            &connection,
            &mut queue,
            &mut probe,
            Duration::from_secs(10),
            |probe| { probe.answered() }
        ),
        "the ordinary capture after the hostile ones was never answered:\n{}",
        compositor.log()
    );
    let capture = probe
        .captures
        .get(&frame.id())
        .expect("the capture was registered");
    let (buffer, _fd) = shm_buffer(
        probe.shm.as_ref().expect("wl_shm"),
        capture.width,
        capture.height,
        capture.stride,
        &qh,
    );
    frame.copy(&buffer);
    assert!(
        pump_until(
            &connection,
            &mut queue,
            &mut probe,
            Duration::from_secs(10),
            |probe| { probe.captures.get(&frame.id()).is_some_and(|c| c.ready) }
        ),
        "the ordinary capture never came back ready:\n{}",
        compositor.log()
    );
    assert!(still_running(&mut compositor));
    buffer.destroy();
    frame.destroy();
}

#[test]
fn a_second_copy_costs_the_client_its_connection_not_the_session() {
    let mut compositor = Compositor::start("sc-twice");
    let (connection, mut queue, mut probe) = connect(&compositor);
    let qh = queue.handle();

    let frame = probe.capture_output(&qh);
    assert!(
        pump_until(
            &connection,
            &mut queue,
            &mut probe,
            Duration::from_secs(10),
            |probe| { probe.answered() }
        ),
        "the first capture was never answered:\n{}",
        compositor.log()
    );
    let capture = probe.captures.get(&frame.id()).expect("the capture");
    let (buffer, _fd) = shm_buffer(
        probe.shm.as_ref().expect("wl_shm"),
        capture.width,
        capture.height,
        capture.stride,
        &qh,
    );
    frame.copy(&buffer);
    assert!(
        pump_until(
            &connection,
            &mut queue,
            &mut probe,
            Duration::from_secs(10),
            |probe| { probe.captures.get(&frame.id()).is_some_and(|c| c.ready) }
        ),
        "the first copy never came back ready:\n{}",
        compositor.log()
    );

    // The protocol's own refusal for a second copy on one frame. It is a
    // protocol error, which costs this client its connection — and nothing
    // else, which is what the fresh capture from a fresh client below pins.
    frame.copy(&buffer);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut dead = false;
    while Instant::now() < deadline {
        if !pump(&connection, &mut queue, &mut probe) {
            dead = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(dead, "the second copy was not a fatal protocol error");
    assert!(still_running(&mut compositor));

    // A fresh client, a fresh capture: the session carries on exactly as
    // it did before the misbehaviour.
    let (connection, mut queue, mut probe) = connect(&compositor);
    let qh = queue.handle();
    let frame = probe.capture_output(&qh);
    assert!(
        pump_until(
            &connection,
            &mut queue,
            &mut probe,
            Duration::from_secs(10),
            |probe| { probe.captures.get(&frame.id()).is_some_and(|c| c.answered) }
        ),
        "a fresh client's capture was never answered after the earlier one \
         was shown the door:\n{}",
        compositor.log()
    );
    assert!(still_running(&mut compositor));
}

#[test]
fn a_buffer_too_small_for_the_frame_is_refused_not_fatal() {
    let mut compositor = Compositor::start("sc-small");
    let (connection, mut queue, mut probe) = connect(&compositor);
    let qh = queue.handle();

    let frame = probe.capture_output(&qh);
    assert!(
        pump_until(
            &connection,
            &mut queue,
            &mut probe,
            Duration::from_secs(10),
            |probe| { probe.answered() }
        ),
        "the capture was never offered a buffer:\n{}",
        compositor.log()
    );
    let capture = probe.captures.get(&frame.id()).expect("the capture");
    // A buffer half the frame's height: legal to create, and too small for
    // the copy the frame is about to make into it.
    let (buffer, _fd) = shm_buffer(
        probe.shm.as_ref().expect("wl_shm"),
        capture.width,
        capture.height / 2,
        capture.stride,
        &qh,
    );
    frame.copy(&buffer);
    assert!(
        pump_until(
            &connection,
            &mut queue,
            &mut probe,
            Duration::from_secs(10),
            |probe| { probe.captures.get(&frame.id()).is_some_and(|c| c.failed) }
        ),
        "the too-small buffer was not refused:\n{}",
        compositor.log()
    );
    assert!(still_running(&mut compositor));

    // The same client may keep asking, and be served.
    probe.captures.clear();
    let frame = probe.capture_output(&qh);
    assert!(
        pump_until(
            &connection,
            &mut queue,
            &mut probe,
            Duration::from_secs(10),
            |probe| { probe.captures.get(&frame.id()).is_some_and(|c| c.answered) }
        ),
        "a capture after the refused one was never answered:\n{}",
        compositor.log()
    );
    assert!(still_running(&mut compositor));
    frame.destroy();
}

#[test]
fn hostile_layer_values_cannot_wedge_the_session() {
    let mut compositor = Compositor::start("layer-extremes");
    let (connection, mut queue, mut probe) = connect(&compositor);
    let qh = queue.handle();

    // Every anchor claimed, margins at the wire's floor, an exclusive zone
    // past any screen, and the keyboard demanded: each of these is a bare
    // `int` on the wire, and together they are the worst a client can name.
    let surface = probe
        .compositor
        .as_ref()
        .expect("wl_compositor")
        .create_surface(&qh, ());
    let layer = probe
        .layer_shell
        .as_ref()
        .expect("zwlr_layer_shell_v1")
        .get_layer_surface(
            &surface,
            None,
            zwlr_layer_shell_v1::Layer::Top,
            "hostile".to_owned(),
            &qh,
            (),
        );
    layer.set_anchor(zwlr_layer_surface_v1::Anchor::all());
    layer.set_margin(i32::MIN, i32::MIN, i32::MIN, i32::MIN);
    layer.set_exclusive_zone(i32::MAX);
    layer.set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::Exclusive);
    layer.set_size(1, 1);
    surface.commit();

    // A configure is what proves the arrangement ran without wedging: the
    // layer's size is computed from anchors and margins, and the answer
    // arriving is the compositor having done that arithmetic and survived.
    assert!(
        pump_until(
            &connection,
            &mut queue,
            &mut probe,
            Duration::from_secs(10),
            |probe| { probe.layer_serials.contains_key(&layer.id()) }
        ),
        "the hostile layer was never configured:\n{}",
        compositor.log()
    );
    assert!(still_running(&mut compositor));

    // A capture while the hostile layer is mapped drives a render pass
    // with its absurd geometry in the tree, which is the other half of the
    // surface a client could break.
    let frame = probe.capture_output(&qh);
    assert!(
        pump_until(
            &connection,
            &mut queue,
            &mut probe,
            Duration::from_secs(10),
            |probe| { probe.captures.get(&frame.id()).is_some_and(|c| c.answered) }
        ),
        "no capture was served with the hostile layer mapped:\n{}",
        compositor.log()
    );
    assert!(still_running(&mut compositor));
    frame.destroy();

    // Taking it away again, and the session still answers for the next
    // client that wants a layer of its own.
    layer.destroy();
    surface.destroy();
    assert!(queue.roundtrip(&mut probe).is_ok());

    let surface = probe
        .compositor
        .as_ref()
        .expect("wl_compositor")
        .create_surface(&qh, ());
    let layer = probe
        .layer_shell
        .as_ref()
        .expect("zwlr_layer_shell_v1")
        .get_layer_surface(
            &surface,
            None,
            zwlr_layer_shell_v1::Layer::Bottom,
            "settled".to_owned(),
            &qh,
            (),
        );
    layer.set_size(320, 32);
    surface.commit();
    assert!(
        pump_until(
            &connection,
            &mut queue,
            &mut probe,
            Duration::from_secs(10),
            |probe| { probe.layer_serials.contains_key(&layer.id()) }
        ),
        "an ordinary layer was never configured after the hostile one:\n{}",
        compositor.log()
    );
    assert!(still_running(&mut compositor));
}

#[test]
fn a_popup_with_hostile_geometry_and_a_dead_parent_leaves_the_session_alive() {
    let mut compositor = Compositor::start("popup-hostile");
    let (connection, mut queue, mut probe) = connect(&compositor);
    let qh = queue.handle();

    // A parent to anchor against, configured so the popup has one to be
    // unconstrained against.
    let parent_surface = probe
        .compositor
        .as_ref()
        .expect("wl_compositor")
        .create_surface(&qh, ());
    let parent_xdg = probe
        .wm_base
        .as_ref()
        .expect("xdg_wm_base")
        .get_xdg_surface(&parent_surface, &qh, ());
    let toplevel = parent_xdg.get_toplevel(&qh, ());
    toplevel.set_app_id("popup-hostile".to_owned());
    toplevel.set_title("popup-hostile".to_owned());
    parent_surface.commit();
    assert!(
        pump_until(
            &connection,
            &mut queue,
            &mut probe,
            Duration::from_secs(10),
            |probe| { probe.configured.contains_key(&parent_xdg.id()) }
        ),
        "the parent was never configured:\n{}",
        compositor.log()
    );

    // The degenerate positioner: an anchor rectangle at the coordinate
    // floor, an offset at it too, and the smallest legal size. The
    // unconstrainer has to subtract both extremes, in i32, and answer.
    let positioner = probe
        .wm_base
        .as_ref()
        .expect("xdg_wm_base")
        .create_positioner(&qh, ());
    positioner.set_size(1, 1);
    positioner.set_anchor_rect(i32::MIN, i32::MIN, 1, 1);
    positioner.set_offset(i32::MIN, i32::MIN);
    positioner.set_anchor(xdg_positioner::Anchor::BottomRight);
    positioner.set_gravity(xdg_positioner::Gravity::BottomRight);

    let popup_surface = probe
        .compositor
        .as_ref()
        .expect("wl_compositor")
        .create_surface(&qh, ());
    let popup_xdg = probe
        .wm_base
        .as_ref()
        .expect("xdg_wm_base")
        .get_xdg_surface(&popup_surface, &qh, ());
    let _popup = popup_xdg.get_popup(Some(&parent_xdg), &positioner, &qh, ());
    positioner.destroy();
    popup_surface.commit();
    assert!(
        pump_until(
            &connection,
            &mut queue,
            &mut probe,
            Duration::from_secs(10),
            |probe| { probe.configured.contains_key(&popup_xdg.id()) }
        ),
        "the hostile popup was never configured:\n{}",
        compositor.log()
    );
    assert!(still_running(&mut compositor));

    // The parent goes away with the popup still alive: the destroy race
    // every popup implementation has to survive. The popup follows it, and
    // the connection does not.
    toplevel.destroy();
    parent_xdg.destroy();
    parent_surface.destroy();
    assert!(queue.roundtrip(&mut probe).is_ok());
    assert!(still_running(&mut compositor));

    // A fresh window and a fresh popup on it: the same session, answering
    // the same questions as before the misbehaviour.
    let parent_surface = probe
        .compositor
        .as_ref()
        .expect("wl_compositor")
        .create_surface(&qh, ());
    let parent_xdg = probe
        .wm_base
        .as_ref()
        .expect("xdg_wm_base")
        .get_xdg_surface(&parent_surface, &qh, ());
    let toplevel = parent_xdg.get_toplevel(&qh, ());
    toplevel.set_app_id("popup-after".to_owned());
    parent_surface.commit();
    assert!(
        pump_until(
            &connection,
            &mut queue,
            &mut probe,
            Duration::from_secs(10),
            |probe| { probe.configured.contains_key(&parent_xdg.id()) }
        ),
        "no window is configured after the hostile popup's parent went:\n{}",
        compositor.log()
    );
    assert!(still_running(&mut compositor));
    toplevel.destroy();
    parent_xdg.destroy();
    parent_surface.destroy();
    popup_surface.destroy();
    let _ = queue.roundtrip(&mut probe);
}

#[test]
fn an_overlay_rect_at_the_wire_floor_cannot_kill_the_frame_build() {
    let mut compositor = Compositor::start("overlay-floor");
    let mut ipc = compositor.connect();

    // A monitor at a positive origin, which is the ordinary side-by-side
    // layout. The control socket bounds its position to ±MAX_LAYOUT_COORD
    // (`output.configure`'s `layout_coord_ok`), and the overlay rectangle
    // is the one wire value left that is not bounded by anything.
    ipc.send(r#"{"type":"output.configure","name":"HEADLESS-1","x":100,"y":0}"#);
    let _ = ipc.drain(Duration::from_secs(2));

    // A rectangle whose origin is as far below the layout as the wire
    // allows, on a monitor whose origin is above zero. The frame build
    // subtracts the monitor's origin from the rectangle's in raw `i32`.
    ipc.send(
        r#"{"type":"shell.overlay","rects":[{"x":-2147483648,"y":0,"width":10,"height":10}]}"#,
    );
    let _ = ipc.drain(Duration::from_secs(2));

    // `set_shell_overlays` marks a frame, and a capture is what builds one
    // headless: `frame_for` runs for every capture the tick services, and
    // the overlay translation inside it is the subtraction under test. The
    // compositor must still be running afterwards: a rectangle this far
    // off the layout is nonsense to drop or clamp, not a reason to take
    // the frame build (and with it, the whole session) down.
    let (connection, mut queue, mut probe) = connect(&compositor);
    let qh = queue.handle();
    let frame = probe.capture_output(&qh);
    assert!(
        pump_until(
            &connection,
            &mut queue,
            &mut probe,
            Duration::from_secs(10),
            |probe| { probe.captures.get(&frame.id()).is_some_and(|c| c.answered) }
        ),
        "the capture that builds the hostile frame was never answered:\n{}",
        compositor.log()
    );
    let capture = probe
        .captures
        .get(&frame.id())
        .cloned()
        .expect("the capture");
    let (buffer, _fd) = shm_buffer(
        probe.shm.as_ref().expect("wl_shm"),
        capture.width,
        capture.height,
        capture.stride,
        &qh,
    );
    frame.copy(&buffer);
    assert!(
        pump_until(
            &connection,
            &mut queue,
            &mut probe,
            Duration::from_secs(10),
            |probe| {
                probe
                    .captures
                    .get(&frame.id())
                    .is_some_and(|c| c.ready || c.failed)
            }
        ),
        "the capture that builds the hostile frame never completed:\n{}",
        compositor.log()
    );
    assert!(
        still_running(&mut compositor),
        "the frame build died on an overlay rectangle at the wire floor:\n{}",
        compositor.log()
    );
    frame.destroy();

    // And an ordinary overlay afterwards still floats above the windows.
    ipc.send(r#"{"type":"shell.overlay","rects":[{"x":10,"y":10,"width":100,"height":50}]}"#);
    let _ = ipc.drain(Duration::from_secs(1));
    assert!(
        still_running(&mut compositor),
        "an ordinary overlay after the hostile one took the session:\n{}",
        compositor.log()
    );
}

#[test]
fn a_window_layout_at_the_wire_floor_cannot_kill_the_hit_test() {
    let mut compositor = Compositor::start("layout-floor");
    let mut ipc = compositor.connect();

    // A window with a geometry whose origin is not the surface's — the
    // client-side-decoration shape, a shadow drawn outside the window —
    // because the hit test subtracts that origin from wherever the shell
    // laid the window out, in raw `i32`.
    let (connection, mut queue, mut probe) = connect(&compositor);
    let qh = queue.handle();
    let surface = probe
        .compositor
        .as_ref()
        .expect("wl_compositor")
        .create_surface(&qh, ());
    let xdg = probe
        .wm_base
        .as_ref()
        .expect("xdg_wm_base")
        .get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg.get_toplevel(&qh, ());
    toplevel.set_app_id("hit-floor".to_owned());
    toplevel.set_title("hit-floor".to_owned());
    surface.commit();
    assert!(
        pump_until(
            &connection,
            &mut queue,
            &mut probe,
            Duration::from_secs(10),
            |probe| { probe.configured.contains_key(&xdg.id()) }
        ),
        "the window was never configured:\n{}",
        compositor.log()
    );
    // The geometry a client with shadows outside its window reports, set
    // the way a real one sets it: after the configure, applied on commit.
    xdg.set_window_geometry(50, 50, 300, 200);
    let (buffer, _fd) = shm_buffer(probe.shm.as_ref().expect("wl_shm"), 400, 250, 400 * 4, &qh);
    surface.attach(Some(&buffer), 0, 0);
    surface.commit();
    assert!(queue.roundtrip(&mut probe).is_ok());

    // The view the shell was told about, which is the id a layout names.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut id = None;
    while id.is_none() && Instant::now() < deadline {
        for message in ipc.drain(Duration::from_millis(200)) {
            if message["type"] == "view.added" && message["app_id"] == "hit-floor" {
                id = message["id"].as_u64();
            }
        }
    }
    let id = id.unwrap_or_else(|| panic!("no view.added for hit-floor:\n{}", compositor.log()));

    // The layout itself: a window the shell placed as far below the layout
    // as the wire allows. Nothing refuses it, and every later pointer move
    // walks it.
    ipc.send(&format!(
        r#"{{"type":"view.layout","id":{id},"x":-2147483648,"y":0,"width":300,"height":200}}"#
    ));
    let _ = ipc.drain(Duration::from_secs(1));

    // A pointer move is what runs the hit test, and the hit test walks
    // every window — `location - window.geometry().loc`, in raw `i32`. The
    // compositor must survive it: a layout the shell miscounted is a window
    // in the wrong place, not a session gone.
    ipc.send(r#"{"type":"input.pointer","x":100,"y":100}"#);
    let _ = ipc.drain(Duration::from_secs(1));
    eprintln!(
        "LOG: {:#?}",
        compositor
            .log()
            .lines()
            .filter(|l| l.contains("placed at")
                || l.contains("panic")
                || l.contains("ipc: from")
                || l.contains("refus"))
            .collect::<Vec<_>>()
    );
    assert!(
        still_running(&mut compositor),
        "the hit test died on a window laid out at the wire floor:\n{}",
        compositor.log()
    );

    // And an ordinary layout afterwards puts the window back where the
    // shell can see it.
    ipc.send(&format!(
        r#"{{"type":"view.layout","id":{id},"x":100,"y":100,"width":300,"height":200}}"#
    ));
    let _ = ipc.drain(Duration::from_secs(1));
    ipc.send(r#"{"type":"input.pointer","x":200,"y":200}"#);
    let _ = ipc.drain(Duration::from_secs(1));
    assert!(
        still_running(&mut compositor),
        "an ordinary layout after the hostile one took the session:\n{}",
        compositor.log()
    );
    buffer.destroy();
    toplevel.destroy();
    xdg.destroy();
    surface.destroy();
    let _ = queue.roundtrip(&mut probe);
}

/// One capture asked for, served, and answered — the whole exchange, so a
/// probe can say "and the frame build survived" with the copy that drives
/// it actually made.
fn capture_to_completion(
    connection: &Connection,
    queue: &mut EventQueue<Probe>,
    probe: &mut Probe,
) -> bool {
    let qh = queue.handle();
    probe.captures.clear();
    let frame = probe.capture_output(&qh);
    if !pump_until(connection, queue, probe, Duration::from_secs(10), |probe| {
        probe.captures.get(&frame.id()).is_some_and(|c| c.answered)
    }) {
        return false;
    }
    let capture = probe
        .captures
        .get(&frame.id())
        .cloned()
        .expect("the capture was registered");
    let (buffer, _fd) = shm_buffer(
        probe.shm.as_ref().expect("wl_shm"),
        capture.width,
        capture.height,
        capture.stride,
        &qh,
    );
    frame.copy(&buffer);
    let done = pump_until(connection, queue, probe, Duration::from_secs(10), |probe| {
        probe
            .captures
            .get(&frame.id())
            .is_some_and(|c| c.ready || c.failed)
    });
    buffer.destroy();
    frame.destroy();
    done
}

#[test]
fn a_layout_clip_and_frame_at_the_wire_floor_cannot_kill_the_frame_build() {
    let mut compositor = Compositor::start("clip-frame-floor");
    // Before the window maps, so the `view.added` it announces is heard
    // rather than missed by a connection that arrives after it.
    let mut ipc = compositor.connect();

    // A monitor at a positive origin, the ordinary side-by-side layout: the
    // frame build subtracts it from the frame rectangle's coordinates in
    // raw `i32`, which is where a frame at the wire's floor lands.
    ipc.send(r#"{"type":"output.configure","name":"HEADLESS-1","x":100,"y":0}"#);
    let _ = ipc.drain(Duration::from_secs(2));

    // A window to lay out: mapped and announced, so the shell's messages
    // name a view that exists.
    let (connection, mut queue, mut probe) = connect(&compositor);
    let qh = queue.handle();
    let surface = probe
        .compositor
        .as_ref()
        .expect("wl_compositor")
        .create_surface(&qh, ());
    let xdg = probe
        .wm_base
        .as_ref()
        .expect("xdg_wm_base")
        .get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg.get_toplevel(&qh, ());
    toplevel.set_app_id("clip-floor".to_owned());
    toplevel.set_title("clip-floor".to_owned());
    surface.commit();
    assert!(
        pump_until(
            &connection,
            &mut queue,
            &mut probe,
            Duration::from_secs(10),
            |probe| { probe.configured.contains_key(&xdg.id()) }
        ),
        "the window was never configured:\n{}",
        compositor.log()
    );
    let (buffer, _fd) = shm_buffer(probe.shm.as_ref().expect("wl_shm"), 200, 100, 800, &qh);
    surface.attach(Some(&buffer), 0, 0);
    surface.commit();
    assert!(queue.roundtrip(&mut probe).is_ok());

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut id = None;
    while id.is_none() && Instant::now() < deadline {
        for message in ipc.drain(Duration::from_millis(200)) {
            if message["type"] == "view.added" && message["app_id"] == "clip-floor" {
                id = message["id"].as_u64();
            }
        }
    }
    let id = id.unwrap_or_else(|| panic!("no view.added for clip-floor:\n{}", compositor.log()));

    // The capture-disallowed case is the one whose frame rectangle reaches
    // the frame build's redaction path.
    ipc.send(&format!(
        r#"{{"type":"view.capture","id":{id},"capture":false}}"#
    ));

    // A frame rectangle at the wire's floor, on the same window: the frame
    // build subtracts the output's origin from it in raw `i32`, inside the
    // redaction bounds.
    ipc.send(&format!(
        r#"{{"type":"view.layout","id":{id},"x":100,"y":100,"width":200,"height":100,"frame":{{"x":-2147483648,"y":100,"width":10,"height":10}}}}"#
    ));
    assert!(
        capture_to_completion(&connection, &mut queue, &mut probe),
        "the capture never completed with a hostile frame in place:\n{}",
        compositor.log()
    );
    assert!(
        still_running(&mut compositor),
        "the frame build died on a frame rectangle at the wire floor:\n{}",
        compositor.log()
    );

    // A clip whose width is *negative*: not the documented "none of this
    // window" — that is zero — but the same intent spelled hostile, and the
    // rectangle the renderer builds from it must still be a legal one.
    ipc.send(&format!(
        r#"{{"type":"view.layout","id":{id},"x":100,"y":100,"width":200,"height":100,"clip":{{"x":100,"y":100,"width":-5,"height":100}}}}"#
    ));
    assert!(
        capture_to_completion(&connection, &mut queue, &mut probe),
        "the capture never completed with a negative clip in place:\n{}",
        compositor.log()
    );
    assert!(
        still_running(&mut compositor),
        "the frame build died on a clip with a negative width:\n{}",
        compositor.log()
    );

    // And an ordinary clip and frame afterwards still crop and border.
    ipc.send(&format!(
        r#"{{"type":"view.layout","id":{id},"x":100,"y":100,"width":200,"height":100,"clip":{{"x":100,"y":100,"width":100,"height":100}},"frame":{{"x":90,"y":90,"width":220,"height":120}}}}"#
    ));
    assert!(
        capture_to_completion(&connection, &mut queue, &mut probe),
        "an ordinary layout after the hostile ones never completed:\n{}",
        compositor.log()
    );
    assert!(still_running(&mut compositor));
    buffer.destroy();
    toplevel.destroy();
    xdg.destroy();
    surface.destroy();
    let _ = queue.roundtrip(&mut probe);
}
