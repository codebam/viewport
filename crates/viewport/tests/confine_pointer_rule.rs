// SPDX-License-Identifier: GPL-3.0-or-later
//
// confine_pointer: a rule holds the cursor inside a focused window.
//
// Hyprland's `confine_pointer` window rule keeps the pointer on a game's
// monitor without the client asking for a constraint of its own. The shell
// resolves the rule and sends `view.confine_pointer`; the compositor's
// pointer_constraint holds the cursor inside the window's box while the
// window is focused. This watches that on a real client: a motion toward a
// far corner leaves the window when no rule confines it, and is clamped to
// the window's edge when one does.
//
// Confinement is resolved from the surface under the pointer's *starting*
// position, so each case first parks the pointer inside the window, then
// asks it to go far away. The client's wl_pointer enter/motion is the proof:
// confined, the cursor is still inside the box at the far edge; free, it has
// left the surface entirely.
//
// harness = false: one binary, re-exec'd as the shell through
// VIEWPORT_SHELL_BIN (it maps a toplevel and sits still), or run as the test.

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
    if std::env::var_os("VIEWPORT_TEST_CONFINE_SHELL").is_some() {
        shell_mode();
    } else {
        test_mode();
    }
}

struct Paint {
    buffer: wl_buffer::WlBuffer,
    _fd: OwnedFd,
}

#[derive(Default)]
struct Client {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    pointer: Option<wl_pointer::WlPointer>,
    windows: Vec<Paint>,
    configured: bool,
    closed: bool,
    entered: bool,
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

fn paint(shm: &wl_shm::WlShm, w: i32, h: i32, color: u32, qh: &QueueHandle<Client>) -> Paint {
    let name = b"viewport-confine\0";
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
    let paint = paint(client.shm.as_ref().expect("wl_shm"), w, h, 0xff30a050, &qh);
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
    client.windows.push(paint);
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
    make_window(&mut client, &mut queue, "confine-shell", 1920, 1080);
    while !client.closed && pump(&connection, &mut queue, &mut client) {
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

/// Move the pointer to a global position and let the events settle.
fn move_pointer(
    ipc: &mut common::Client,
    connection: &Connection,
    queue: &mut EventQueue<Client>,
    client: &mut Client,
    x: f64,
    y: f64,
) {
    ipc.send(&format!(r#"{{"type":"input.pointer","x":{x},"y":{y}}}"#));
    let deadline = Instant::now() + Duration::from_millis(200);
    while Instant::now() < deadline {
        assert!(pump(connection, queue, client), "window connection died");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn test_mode() {
    let exe = std::env::current_exe().expect("current exe");
    let compositor = Compositor::builder("confine-pointer")
        .prefix("viewport-confine-pointer")
        .env("VIEWPORT_SHELL_BIN", &exe)
        .env("VIEWPORT_TEST_CONFINE_SHELL", "1")
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

    // The window sits at (100, 100) sized 400x300: surface-local (0..400,
    // 0..300), global (100..500, 100..400). Its centre is global (300, 250).
    make_window(&mut client, &mut queue, "confine-window", 400, 300);
    let id = view_id_for(&mut ipc, "confine-window");
    ipc.send(&format!(
        r#"{{"type":"view.layout","id":{id},"x":100,"y":100,"width":400,"height":300}}"#
    ));
    ipc.send(&format!(r#"{{"type":"view.focus","id":{id}}}"#));
    assert_eq!(ipc.expect("view.focused")["id"].as_u64(), Some(id));

    // Control — no rule: a motion to a far corner leaves the window.
    move_pointer(&mut ipc, &connection, &mut queue, &mut client, 300.0, 250.0);
    assert!(client.entered, "the pointer never entered the window");
    move_pointer(
        &mut ipc,
        &connection,
        &mut queue,
        &mut client,
        1900.0,
        1000.0,
    );
    assert!(
        !client.entered,
        "with no rule the pointer should have left the window:\n{}",
        compositor.log()
    );
    println!("ok: without the rule a far motion leaves the window");

    // The rule on: the same motion is clamped to the window's far edge. The
    // pointer starts inside (parked at the centre), so the confinement is
    // resolved from the window and holds the cursor in the box.
    ipc.send(&format!(
        r#"{{"type":"view.confine_pointer","id":{id},"confine":true}}"#
    ));
    move_pointer(&mut ipc, &connection, &mut queue, &mut client, 300.0, 250.0);
    assert!(client.entered, "the pointer never re-entered the window");
    move_pointer(
        &mut ipc,
        &connection,
        &mut queue,
        &mut client,
        1900.0,
        1000.0,
    );
    assert!(
        client.entered,
        "the rule did not confine the pointer — it left the window:\n{}",
        compositor.log()
    );
    let (x, y) = client.pointer_pos.expect("a pointer position");
    assert!(
        (350.0..400.0).contains(&x) && (250.0..300.0).contains(&y),
        "the confined pointer landed at ({x}, {y}), not the window's far edge"
    );
    println!("ok: the rule clamps a far motion to the window's edge");

    // The rule off: freedom returns.
    ipc.send(&format!(
        r#"{{"type":"view.confine_pointer","id":{id},"confine":false}}"#
    ));
    move_pointer(&mut ipc, &connection, &mut queue, &mut client, 300.0, 250.0);
    move_pointer(
        &mut ipc,
        &connection,
        &mut queue,
        &mut client,
        1900.0,
        1000.0,
    );
    assert!(
        !client.entered,
        "turning the rule off did not release the pointer:\n{}",
        compositor.log()
    );
    println!("ok: turning the rule off releases the pointer");
}
