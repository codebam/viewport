// SPDX-License-Identifier: GPL-3.0-or-later
//
// fadePopups: a menu that arrives fades in, and then is simply there.
//
// The regression this guards: the fade map is global state, but it was pruned
// once per output, against only that output's popups. On a desk with two
// monitors each output's frame builder threw away the other's popup fades every
// frame, so a popup's fade-in restarted from alpha 0 forever and the menu never
// became visible — a Chrome toolbar context menu that simply does not appear.
// One output never pruned anything it was drawing, so the bug was invisible on
// a single monitor and decisive on two.
//
// This maps a real window and a real solid-colour xdg_popup on the first
// output, then reads the screen back through wlr-screencopy while keeping a
// capture pending on every output (which is what drives a frame build per
// output under the headless backend, exactly as a vblank does on DRM). The
// popup's colour must show up once its fade is spent. With the per-output prune
// it never does on two outputs; with the fix it does on one and on two.
//
// harness = false: one binary, re-exec'd as the shell through
// VIEWPORT_SHELL_BIN (it maps a toplevel and sits still), or run as the test.

mod common;

use common::Compositor;
use std::collections::HashMap;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use wayland_client::backend::{ObjectId, WaylandError};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_output, wl_registry, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::xdg::shell::client::{
    xdg_popup, xdg_positioner, xdg_surface, xdg_toplevel, xdg_wm_base,
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1, zwlr_screencopy_manager_v1,
};

/// The popup's colour, and the window's: apart from either other and from the
/// shell's backdrop, so a green pixel can only be the popup.
const POPUP_COLOR: u32 = 0xff00_ff00;
const WINDOW_COLOR: u32 = 0xffff_0000;
const SHELL_COLOR: u32 = 0xffa0_5030;
const POPUP_W: i32 = 200;
const POPUP_H: i32 = 150;

fn main() {
    if std::env::var_os("VIEWPORT_TEST_POPUP_SHELL").is_some() {
        shell_mode();
    } else {
        test_mode();
    }
}

struct Paint {
    buffer: wl_buffer::WlBuffer,
    _fd: OwnedFd,
}

/// One screencopy frame in flight: the buffer it will land in, and whether it
/// has arrived. Keyed by the frame's object id in `Client::captures`.
struct Capture {
    output_index: usize,
    width: i32,
    height: i32,
    stride: i32,
    buffer: Option<wl_buffer::WlBuffer>,
    fd: Option<OwnedFd>,
    ready: bool,
    failed: bool,
}

#[derive(Default)]
struct Client {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    screencopy: Option<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1>,
    outputs: Vec<wl_output::WlOutput>,

    surface: Option<wl_surface::WlSurface>,
    xdg_surface: Option<xdg_surface::XdgSurface>,
    toplevel: Option<xdg_toplevel::XdgToplevel>,
    paint: Option<Paint>,
    configured: bool,

    popup_surface: Option<wl_surface::WlSurface>,
    popup_xdg: Option<xdg_surface::XdgSurface>,
    popup_role: Option<xdg_popup::XdgPopup>,
    popup_paint: Option<Paint>,
    popup_configured: bool,

    captures: HashMap<ObjectId, Capture>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for Client {
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
            "wl_output" => state
                .outputs
                .push(registry.bind(name, version.min(4), qh, ())),
            "zwlr_screencopy_manager_v1" => {
                state.screencopy = Some(registry.bind(name, version.min(3), qh, ()))
            }
            _ => {}
        }
    }
}

delegate_noop!(Client: ignore wl_compositor::WlCompositor);
delegate_noop!(Client: ignore wl_shm::WlShm);
delegate_noop!(Client: ignore wl_shm_pool::WlShmPool);
delegate_noop!(Client: ignore wl_surface::WlSurface);
delegate_noop!(Client: ignore wl_buffer::WlBuffer);
delegate_noop!(Client: ignore wl_output::WlOutput);
delegate_noop!(Client: ignore zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1);
delegate_noop!(Client: ignore xdg_popup::XdgPopup);
delegate_noop!(Client: ignore xdg_positioner::XdgPositioner);

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for Client {
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

impl Dispatch<xdg_surface::XdgSurface, ()> for Client {
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
            // The popup's xdg_surface is a different object from the window's;
            // tell them apart by which one this is.
            if state.popup_xdg.as_ref() == Some(surface) {
                state.popup_configured = true;
            } else {
                state.configured = true;
            }
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for Client {
    fn event(
        _: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        _: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1, ()> for Client {
    fn event(
        state: &mut Self,
        frame: &zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let Some(cap) = state.captures.get_mut(&frame.id()) else {
            return;
        };
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format: _,
                width,
                height,
                stride,
            } => {
                cap.width = width as i32;
                cap.height = height as i32;
                cap.stride = stride as i32;
                let size = stride as usize * height as usize;
                let name = b"popup-fade-cap\0";
                let fd = unsafe { libc::memfd_create(name.as_ptr() as *const libc::c_char, 0) };
                assert!(fd >= 0, "memfd_create failed");
                let fd = unsafe { OwnedFd::from_raw_fd(fd) };
                assert_eq!(
                    unsafe { libc::ftruncate(fd.as_raw_fd(), size as libc::off_t) },
                    0
                );
                let shm = state.shm.as_ref().expect("wl_shm");
                let pool = shm.create_pool(
                    unsafe { BorrowedFd::borrow_raw(fd.as_raw_fd()) },
                    size as i32,
                    qh,
                    (),
                );
                let buffer = pool.create_buffer(
                    0,
                    width as i32,
                    height as i32,
                    stride as i32,
                    wl_shm::Format::Xrgb8888,
                    qh,
                    (),
                );
                pool.destroy();
                cap.buffer = Some(buffer);
                cap.fd = Some(fd);
            }
            zwlr_screencopy_frame_v1::Event::BufferDone => {
                if let Some(buffer) = cap.buffer.as_ref() {
                    frame.copy(buffer);
                }
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => cap.ready = true,
            zwlr_screencopy_frame_v1::Event::Failed => cap.failed = true,
            _ => {}
        }
    }
}

fn paint(shm: &wl_shm::WlShm, w: i32, h: i32, color: u32, qh: &QueueHandle<Client>) -> Paint {
    let name = b"popup-fade\0";
    let fd = unsafe { libc::memfd_create(name.as_ptr() as *const libc::c_char, 0) };
    assert!(fd >= 0, "memfd_create failed");
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let stride = w as usize * 4;
    let size = stride * h as usize;
    assert_eq!(
        unsafe { libc::ftruncate(fd.as_raw_fd(), size as libc::off_t) },
        0
    );
    let pixels: Vec<u32> = vec![color; w as usize * h as usize];
    let bytes =
        unsafe { std::slice::from_raw_parts(pixels.as_ptr() as *const u8, pixels.len() * 4) };
    let mut written = 0usize;
    while written < bytes.len() {
        let n = unsafe {
            libc::pwrite(
                fd.as_raw_fd(),
                bytes[written..].as_ptr() as *const libc::c_void,
                bytes.len() - written,
                written as libc::off_t,
            )
        };
        assert!(n > 0, "pwrite failed");
        written += n as usize;
    }
    let pool = shm.create_pool(
        unsafe { BorrowedFd::borrow_raw(fd.as_raw_fd()) },
        size as i32,
        qh,
        (),
    );
    let buffer = pool.create_buffer(0, w, h, stride as i32, wl_shm::Format::Argb8888, qh, ());
    pool.destroy();
    Paint { buffer, _fd: fd }
}

fn pump(connection: &Connection, queue: &mut EventQueue<Client>, client: &mut Client) -> bool {
    if let Some(guard) = connection.prepare_read() {
        if let Err(err) = guard.read() {
            let empty =
                matches!(&err, WaylandError::Io(e) if e.kind() == std::io::ErrorKind::WouldBlock);
            if !empty {
                return false;
            }
        }
    }
    queue.dispatch_pending(client).expect("dispatch");
    connection.flush().is_ok()
}

fn shell_mode() {
    let connection = Connection::connect_to_env().expect("the shell's WAYLAND_SOCKET");
    let mut queue = connection.new_event_queue();
    let qh = queue.handle();
    connection.display().get_registry(&qh, ());
    let mut client = Client::default();
    queue.roundtrip(&mut client).expect("globals");
    let surface = client
        .compositor
        .as_ref()
        .expect("wl_compositor")
        .create_surface(&qh, ());
    let xdg = client
        .wm_base
        .as_ref()
        .expect("xdg_wm_base")
        .get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg.get_toplevel(&qh, ());
    toplevel.set_app_id("popup-shell".to_owned());
    toplevel.set_title("popup-shell".to_owned());
    let paint = paint(
        client.shm.as_ref().expect("wl_shm"),
        1920,
        1080,
        SHELL_COLOR,
        &qh,
    );
    surface.commit();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !client.configured && Instant::now() < deadline {
        queue.roundtrip(&mut client).expect("shell configure");
    }
    surface.attach(Some(&paint.buffer), 0, 0);
    surface.commit();
    queue.roundtrip(&mut client).expect("shell commit");
    loop {
        if !pump(&connection, &mut queue, &mut client) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn view_id_for(ipc: &mut common::Client, app_id: &str) -> u64 {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        for message in ipc.drain(Duration::from_millis(200)) {
            if message["type"] == "view.added" && message["app_id"] == app_id {
                return message["id"].as_u64().expect("view id");
            }
        }
    }
    panic!("no view.added for {app_id}");
}

/// Map the window under test, place it at a known rectangle, focus it.
fn place_window(
    ipc: &mut common::Client,
    client: &mut Client,
    queue: &mut EventQueue<Client>,
) -> u64 {
    let qh = queue.handle();
    let surface = client
        .compositor
        .as_ref()
        .expect("wl_compositor")
        .create_surface(&qh, ());
    let xdg = client
        .wm_base
        .as_ref()
        .expect("xdg_wm_base")
        .get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg.get_toplevel(&qh, ());
    toplevel.set_app_id("popup-window".to_owned());
    toplevel.set_title("popup-window".to_owned());
    let paint = paint(
        client.shm.as_ref().expect("wl_shm"),
        400,
        300,
        WINDOW_COLOR,
        &qh,
    );
    surface.commit();
    client.configured = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    while !client.configured && Instant::now() < deadline {
        queue.roundtrip(client).expect("window configure");
    }
    assert!(client.configured, "the window was never configured");
    surface.attach(Some(&paint.buffer), 0, 0);
    surface.commit();
    queue.roundtrip(client).expect("window commit");
    client.surface = Some(surface);
    client.xdg_surface = Some(xdg);
    client.toplevel = Some(toplevel);
    client.paint = Some(paint);

    let id = view_id_for(ipc, "popup-window");
    ipc.send(&format!(
        r#"{{"type":"view.layout","id":{id},"x":100,"y":100,"width":400,"height":300}}"#
    ));
    ipc.send(&format!(r#"{{"type":"view.focus","id":{id}}}"#));
    assert_eq!(ipc.expect("view.focused")["id"].as_u64(), Some(id));
    let deadline = Instant::now() + Duration::from_millis(300);
    while Instant::now() < deadline {
        let _ = queue.roundtrip(client);
        std::thread::sleep(Duration::from_millis(20));
    }
    id
}

/// A solid-colour popup anchored inside the window, the way a menu is.
fn open_popup(client: &mut Client, queue: &mut EventQueue<Client>) {
    let qh = queue.handle();
    let wm_base = client.wm_base.as_ref().expect("xdg_wm_base");
    let positioner = wm_base.create_positioner(&qh, ());
    positioner.set_size(POPUP_W, POPUP_H);
    positioner.set_anchor_rect(50, 50, 1, 1);
    positioner.set_anchor(xdg_positioner::Anchor::TopLeft);
    positioner.set_gravity(xdg_positioner::Gravity::BottomRight);
    let surface = client
        .compositor
        .as_ref()
        .expect("wl_compositor")
        .create_surface(&qh, ());
    let xdg = wm_base.get_xdg_surface(&surface, &qh, ());
    // Recorded before the configure wait so the xdg_surface dispatch can tell
    // this popup's configure from the window's.
    client.popup_surface = Some(surface.clone());
    client.popup_xdg = Some(xdg.clone());
    let parent = client
        .xdg_surface
        .as_ref()
        .expect("the window's xdg_surface");
    let popup = xdg.get_popup(Some(parent), &positioner, &qh, ());
    xdg.set_window_geometry(0, 0, POPUP_W, POPUP_H);
    positioner.destroy();
    surface.commit();
    client.popup_configured = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    while !client.popup_configured && Instant::now() < deadline {
        queue.roundtrip(client).expect("popup configure");
    }
    assert!(client.popup_configured, "the popup was never configured");
    let paint = paint(
        client.shm.as_ref().expect("wl_shm"),
        POPUP_W,
        POPUP_H,
        POPUP_COLOR,
        &qh,
    );
    surface.attach(Some(&paint.buffer), 0, 0);
    surface.damage_buffer(0, 0, POPUP_W, POPUP_H);
    surface.commit();
    queue.roundtrip(client).expect("popup commit");
    client.popup_paint = Some(paint);
    client.popup_role = Some(popup);
}

/// Ask every output for a frame at once, so each one's frame builder runs in
/// the same headless tick — the cross-output prune this test is about.
fn request_captures(client: &mut Client, queue: &mut EventQueue<Client>) {
    let qh = queue.handle();
    let manager = client.screencopy.as_ref().expect("screencopy manager");
    let outputs: Vec<wl_output::WlOutput> = client.outputs.clone();
    for (index, output) in outputs.iter().enumerate() {
        let frame = manager.capture_output(0, output, &qh, ());
        client.captures.insert(
            frame.id(),
            Capture {
                output_index: index,
                width: 0,
                height: 0,
                stride: 0,
                buffer: None,
                fd: None,
                ready: false,
                failed: false,
            },
        );
    }
}

/// Dispatch until every pending capture is ready (or failed), bounded.
fn wait_captures(
    connection: &Connection,
    queue: &mut EventQueue<Client>,
    client: &mut Client,
) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let pending = client
            .captures
            .values()
            .any(|cap| !cap.ready && !cap.failed);
        if !pending {
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        if !pump(connection, queue, client) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// How many pixels of the first output's capture are the popup's colour.
fn green_count(client: &Client) -> usize {
    let want = POPUP_COLOR & 0x00ff_ffff;
    let mut count = 0usize;
    for cap in client.captures.values() {
        if cap.output_index != 0 || !cap.ready {
            continue;
        }
        let Some(fd) = cap.fd.as_ref() else { continue };
        let size = cap.stride as usize * cap.height as usize;
        let mut bytes = vec![0u8; size];
        let mut read = 0usize;
        while read < size {
            let n = unsafe {
                libc::pread(
                    fd.as_raw_fd(),
                    bytes[read..].as_mut_ptr() as *mut libc::c_void,
                    size - read,
                    read as libc::off_t,
                )
            };
            if n <= 0 {
                break;
            }
            read += n as usize;
        }
        for pixel in bytes.as_chunks::<4>().0 {
            let value = u32::from_le_bytes(*pixel);
            if value & 0x00ff_ffff == want {
                count += 1;
            }
        }
    }
    count
}

fn clear_captures(client: &mut Client) {
    for (_, cap) in client.captures.drain() {
        if let Some(buffer) = cap.buffer {
            buffer.destroy();
        }
    }
}

fn test_mode() {
    // One output: the fade completes and the menu shows. Two outputs: the same
    // must hold — the regression was that the second output's frame builder
    // reset the first's popup fade every frame, so it never showed.
    run_case("one", false);
    run_case("two", true);
}

fn run_case(tag: &str, second_output: bool) {
    let exe = std::env::current_exe().expect("current exe");
    let dir = std::env::temp_dir().join(format!("viewport-popup-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("test dir");
    let config = dir.join("config.json");
    // A short, explicit fade so the test does not wait on the default.
    std::fs::write(&config, r#"{"motion":{"fade_popups":{"duration":150}}}"#)
        .expect("write config");

    let compositor = Compositor::builder(tag)
        .prefix("viewport-popup-fade")
        .owning(dir.clone())
        .arg("--config")
        .arg(&config)
        .env("VIEWPORT_SHELL_BIN", &exe)
        .env("VIEWPORT_TEST_POPUP_SHELL", "1")
        .env("VIEWPORT_LOG", "viewport=debug")
        .start();
    assert!(
        compositor.saw("shell 0: configuring it to", Duration::from_secs(10)),
        "the fake shell never mapped:\n{}",
        compositor.log()
    );

    let display = compositor
        .wayland_display()
        .expect("the compositor never announced a Wayland display");
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR");
    let connection = Connection::from_socket(
        UnixStream::connect(PathBuf::from(runtime).join(display)).expect("Wayland socket"),
    )
    .expect("Wayland connection");
    let mut queue = connection.new_event_queue();
    let qh = queue.handle();
    connection.display().get_registry(&qh, ());
    let mut client = Client::default();
    queue.roundtrip(&mut client).expect("globals");
    queue.roundtrip(&mut client).expect("output globals");
    let mut ipc = compositor.connect();

    if second_output {
        ipc.send(r#"{"type":"output.test_add"}"#);
        // The new output's wl_output global arrives on the registry; wait for it.
        let deadline = Instant::now() + Duration::from_secs(5);
        while client.outputs.len() < 2 && Instant::now() < deadline {
            let _ = queue.roundtrip(&mut client);
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            client.outputs.len() >= 2,
            "the second output never arrived: {} output(s)",
            client.outputs.len()
        );
    }

    place_window(&mut ipc, &mut client, &mut queue);
    open_popup(&mut client, &mut queue);

    // Watch the screen for longer than the fade: the popup's colour must appear
    // once the fade is spent. Keep a capture on every output the whole time.
    let mut best = 0usize;
    let deadline = Instant::now() + Duration::from_millis(900);
    while Instant::now() < deadline {
        request_captures(&mut client, &mut queue);
        if !wait_captures(&connection, &mut queue, &mut client) {
            panic!("a capture never settled ({tag}):\n{}", compositor.log());
        }
        let failed = client.captures.values().any(|cap| cap.failed);
        assert!(!failed, "a capture failed ({tag}):\n{}", compositor.log());
        best = best.max(green_count(&client));
        clear_captures(&mut client);
        if best > 5000 {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    assert!(
        best > 5000,
        "the popup never became visible on the {tag}-output desk: at most {best} \
         green pixels in any frame (the fade was reset every frame):\n{}",
        compositor.log()
    );
    println!("ok: the popup fades in and stays visible on the {tag}-output desk");
}
