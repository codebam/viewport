// SPDX-License-Identifier: GPL-3.0-or-later
//
// A workspace switch warps the pointer only when the policy asks for it.
//
// `cursor.warp_on_change_workspace` is 0 (off, the pointer stays where it
// was), 1 (warp to the window the shell focuses on the new workspace) or 2
// (force, even when `cursor.no_warps` is set). The compositor never warped on
// its own before this, so the default must leave the pointer alone, and the
// warp must land on the centre of the focused window — observed here as the
// wl_pointer enter/motion a real client receives, not inferred from a flag.
//
// The switch is driven the way the shell drives it: `workspace.list` with the
// active set moving from one id to another (published from inside the shell's
// relayout), then `view.focus` for the first window on the new workspace. The
// compositor arms the warp on the active-set change and pays it on that focus.
//
// The shell is this same binary re-exec'd through VIEWPORT_SHELL_BIN: it maps
// a toplevel so the shell slot is occupied by a client that behaves, then
// sits still — the warp is compositor work and needs no redraw pressure.
//
// harness = false because one binary plays both parts: with
// VIEWPORT_TEST_WARP_SHELL set it is the shell, without it the test.

mod common;

use common::Compositor;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use wayland_client::backend::WaylandError;
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

fn main() {
    if std::env::var_os("VIEWPORT_TEST_WARP_SHELL").is_some() {
        shell_mode();
    } else {
        test_mode();
    }
}

/// A buffer and the fd its pool was made from, kept alive together.
struct Paint {
    buffer: wl_buffer::WlBuffer,
    _fd: OwnedFd,
}

struct Window {
    _surface: wl_surface::WlSurface,
    _xdg: xdg_surface::XdgSurface,
    _toplevel: xdg_toplevel::XdgToplevel,
    _paint: Paint,
}

#[derive(Default)]
struct Client {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    pointer: Option<wl_pointer::WlPointer>,
    windows: Vec<Window>,
    configured: bool,
    closed: bool,
    /// Whether the pointer is currently inside one of this client's surfaces.
    entered: bool,
    /// The last enter/motion position, in surface-local coordinates.
    pointer_pos: Option<(f64, f64)>,
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
            "wl_seat" => {
                // The headless seat always has a pointer; asking for it at
                // bind time rather than waiting on capabilities keeps the
                // test off the bitflags spelling entirely.
                let seat: wl_seat::WlSeat = registry.bind(name, version.min(5), qh, ());
                state.pointer = Some(seat.get_pointer(qh, ()));
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
delegate_noop!(Client: ignore wl_seat::WlSeat);

impl Dispatch<wl_pointer::WlPointer, ()> for Client {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                surface_x,
                surface_y,
                ..
            } => {
                state.entered = true;
                state.pointer_pos = Some((surface_x, surface_y));
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                state.pointer_pos = Some((surface_x, surface_y));
            }
            wl_pointer::Event::Leave { .. } => {
                state.entered = false;
            }
            _ => {}
        }
    }
}

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
            state.configured = true;
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for Client {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_toplevel::Event::Close = event {
            state.closed = true;
        }
    }
}

/// A solid-colour shm buffer, written with pwrite rather than a mapping.
fn paint(shm: &wl_shm::WlShm, w: i32, h: i32, color: u32, qh: &QueueHandle<Client>) -> Paint {
    let name = b"viewport-warp\0";
    let fd = unsafe { libc::memfd_create(name.as_ptr() as *const libc::c_char, 0) };
    assert!(fd >= 0, "memfd_create failed");
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let stride = w as usize * 4;
    let size = stride * h as usize;
    assert_eq!(
        unsafe { libc::ftruncate(fd.as_raw_fd(), size as libc::off_t) },
        0,
        "ftruncate failed"
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

/// Map one toplevel and wait for its initial configure.
fn make_window(client: &mut Client, queue: &mut EventQueue<Client>, app_id: &str, w: i32, h: i32) {
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
    toplevel.set_app_id(app_id.to_owned());
    toplevel.set_title(app_id.to_owned());
    let paint = paint(client.shm.as_ref().expect("wl_shm"), w, h, 0xff3050a0, &qh);
    surface.commit();
    client.configured = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    while !client.configured && Instant::now() < deadline {
        queue.roundtrip(client).expect("initial configure");
    }
    assert!(client.configured, "{app_id} was never configured");
    surface.attach(Some(&paint.buffer), 0, 0);
    surface.commit();
    queue.roundtrip(client).expect("post-configure commit");
    client.windows.push(Window {
        _surface: surface,
        _xdg: xdg,
        _toplevel: toplevel,
        _paint: paint,
    });
}

/// Read whatever arrived without blocking, dispatch it, flush.
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

/// The shell half: occupy the shell slot with a well-behaved client, then sit
/// still. The warp needs no redraw pressure, so there is no commit loop here.
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
    toplevel.set_app_id("warp-shell".to_owned());
    let paint = paint(
        client.shm.as_ref().expect("wl_shm"),
        1920,
        1080,
        0xff202020,
        &qh,
    );
    surface.commit();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !client.configured && Instant::now() < deadline {
        queue.roundtrip(&mut client).expect("shell configure");
    }
    if client.configured {
        surface.attach(Some(&paint.buffer), 0, 0);
        surface.commit();
    }
    while !client.closed && pump(&connection, &mut queue, &mut client) {
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Wait for the fresh view.added of one app_id.
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

/// Park the pointer in a corner well outside the window, settle, and clear
/// the observation so the next phase measures only what the switch does.
fn park_pointer(
    ipc: &mut common::Client,
    connection: &Connection,
    queue: &mut EventQueue<Client>,
    client: &mut Client,
) {
    ipc.send(r#"{"type":"input.pointer","x":10,"y":10}"#);
    let deadline = Instant::now() + Duration::from_millis(200);
    while Instant::now() < deadline {
        assert!(pump(connection, queue, client), "window connection died");
        std::thread::sleep(Duration::from_millis(20));
    }
    client.entered = false;
    client.pointer_pos = None;
}

/// Drive one workspace switch the way the shell does: publish the active set
/// moving from `from` to `to`, then focus the window on the new workspace.
fn switch_workspace(ipc: &mut common::Client, from: &str, to: &str, id: u64) {
    ipc.send(&format!(
        r#"{{"type":"workspace.list","workspaces":[{{"id":"{from}","active":true}}]}}"#
    ));
    ipc.send(&format!(
        r#"{{"type":"workspace.list","workspaces":[{{"id":"{from}","active":false}},{{"id":"{to}","active":true}}]}}"#
    ));
    ipc.send(&format!(r#"{{"type":"view.focus","id":{id}}}"#));
    let focused = ipc.expect("view.focused");
    assert_eq!(focused["id"].as_u64(), Some(id), "the switch never focused");
}

/// Pump the window connection for a beat so any warp the switch owed arrives.
fn settle(connection: &Connection, queue: &mut EventQueue<Client>, client: &mut Client) {
    let deadline = Instant::now() + Duration::from_millis(300);
    while Instant::now() < deadline {
        assert!(pump(connection, queue, client), "window connection died");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The window is placed at (100, 100) sized 400x300, so its centre is global
/// (300, 250) and surface-local (200, 150). A warp lands the pointer there.
fn assert_warped_to_centre(client: &Client) {
    assert!(
        client.entered,
        "the pointer never entered the window — no warp landed"
    );
    let (x, y) = client.pointer_pos.expect("a pointer position");
    assert!(
        (x - 200.0).abs() < 1.0 && (y - 150.0).abs() < 1.0,
        "the warp landed at ({x}, {y}), not the window centre (200, 150)"
    );
}

fn test_mode() {
    let exe = std::env::current_exe().expect("current exe");
    let compositor = Compositor::builder("cursor-warp")
        .prefix("viewport-cursor-warp")
        .env("VIEWPORT_SHELL_BIN", &exe)
        .env("VIEWPORT_TEST_WARP_SHELL", "1")
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
    assert!(client.pointer.is_some(), "the seat never offered a pointer");

    let mut ipc = compositor.connect();
    make_window(&mut client, &mut queue, "warp-window", 400, 300);
    let id = view_id_for(&mut ipc, "warp-window");
    ipc.send(&format!(
        r#"{{"type":"view.layout","id":{id},"x":100,"y":100,"width":400,"height":300}}"#
    ));
    ipc.send(&format!(r#"{{"type":"view.focus","id":{id}}}"#));
    assert_eq!(ipc.expect("view.focused")["id"].as_u64(), Some(id));
    // A baseline active workspace, so the first phase's switch is a real move
    // of the active set rather than the empty world becoming populated.
    ipc.send(r#"{"type":"workspace.list","workspaces":[{"id":"1","active":true}]}"#);

    // Phase A — the default (0): a switch moves focus, not the pointer.
    park_pointer(&mut ipc, &connection, &mut queue, &mut client);
    switch_workspace(&mut ipc, "1", "2", id);
    settle(&connection, &mut queue, &mut client);
    assert!(
        !client.entered,
        "the pointer warped with the policy off:\n{}",
        compositor.log()
    );
    println!("ok: the default leaves the pointer where it was");

    // Phase B — mode 1: the switch warps to the centre of the focused window.
    ipc.send(r#"{"type":"config.cursor","warp_on_change_workspace":1}"#);
    ipc.expect("config");
    park_pointer(&mut ipc, &connection, &mut queue, &mut client);
    switch_workspace(&mut ipc, "2", "3", id);
    settle(&connection, &mut queue, &mut client);
    assert_warped_to_centre(&client);
    println!("ok: mode 1 warps to the focused window's centre");

    // Phase C — mode 1 under no_warps: the master switch suppresses it.
    ipc.send(r#"{"type":"config.cursor","no_warps":true}"#);
    ipc.expect("config");
    park_pointer(&mut ipc, &connection, &mut queue, &mut client);
    switch_workspace(&mut ipc, "3", "4", id);
    settle(&connection, &mut queue, &mut client);
    assert!(
        !client.entered,
        "no_warps did not suppress the mode-1 warp:\n{}",
        compositor.log()
    );
    println!("ok: no_warps suppresses the mode-1 warp");

    // Phase D — mode 2 forces past no_warps, which is still set.
    ipc.send(r#"{"type":"config.cursor","warp_on_change_workspace":2}"#);
    ipc.expect("config");
    park_pointer(&mut ipc, &connection, &mut queue, &mut client);
    switch_workspace(&mut ipc, "4", "5", id);
    settle(&connection, &mut queue, &mut client);
    assert_warped_to_centre(&client);
    println!("ok: mode 2 forces the warp past no_warps");
}
