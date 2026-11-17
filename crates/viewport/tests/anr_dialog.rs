// SPDX-License-Identifier: GPL-3.0-or-later
//
// misc:enable_anr_dialog — a window that stops answering is asked about, and
// can be force-quit.
//
// The compositor pings each mapped xdg window on a timer; a client that does
// not pong within the deadline is flagged and the shell sent `view.anr`. This
// is a real client that deliberately ignores the ping: the test waits for the
// flag, answers `view.anr_resolve` with `quit`, and watches the compositor
// disconnect the client (Smithay's `unresponsive`, the xdg_wm_base error).
//
// The wait is the ANR deadline itself — several seconds — so this is a slow
// test by nature, not by accident.
//
// harness = false: one binary, re-exec'd as the shell through
// VIEWPORT_SHELL_BIN (it maps a toplevel and pongs like a good client), or run
// as the test, whose window does not pong.

mod common;

use common::Compositor;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use wayland_client::backend::WaylandError;
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

fn main() {
    if std::env::var_os("VIEWPORT_TEST_ANR_SHELL").is_some() {
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
    paints: Vec<Paint>,
    configured: bool,
    closed: bool,
    /// The shell pongs; the test's stuck window does not. That difference is
    /// the whole experiment.
    pong: bool,
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
            _ => {}
        }
    }
}

delegate_noop!(Client: ignore wl_compositor::WlCompositor);
delegate_noop!(Client: ignore wl_shm::WlShm);
delegate_noop!(Client: ignore wl_shm_pool::WlShmPool);
delegate_noop!(Client: ignore wl_surface::WlSurface);
delegate_noop!(Client: ignore wl_buffer::WlBuffer);

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for Client {
    fn event(
        state: &mut Self,
        wm_base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            if state.pong {
                wm_base.pong(serial);
            }
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
    let name = b"viewport-anr\0";
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
    let paint = paint(client.shm.as_ref().expect("wl_shm"), w, h, 0xffa03030, &qh);
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
    client.paints.push(paint);
}

/// Read and dispatch without blocking. Returns false once the connection is
/// gone — a read error other than WouldBlock, or a dispatch error (the protocol
/// error a force quit posts).
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
    if queue.dispatch_pending(client).is_err() {
        return false;
    }
    connection.flush().is_ok()
}

fn shell_mode() {
    let connection = Connection::connect_to_env().expect("the shell's WAYLAND_SOCKET");
    let mut queue = connection.new_event_queue();
    let qh = queue.handle();
    connection.display().get_registry(&qh, ());
    let mut client = Client {
        pong: true,
        ..Default::default()
    };
    queue.roundtrip(&mut client).expect("globals");
    make_window(&mut client, &mut queue, "anr-shell", 1920, 1080);
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

fn test_mode() {
    let exe = std::env::current_exe().expect("current exe");
    let compositor = Compositor::builder("anr-dialog")
        .prefix("viewport-anr-dialog")
        .env("VIEWPORT_SHELL_BIN", &exe)
        .env("VIEWPORT_TEST_ANR_SHELL", "1")
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
    // The stuck window: it never pongs.
    let mut client = Client::default();
    queue.roundtrip(&mut client).expect("globals");
    let mut ipc = compositor.connect();

    make_window(&mut client, &mut queue, "stuck-window", 400, 300);
    let id = view_id_for(&mut ipc, "stuck-window");
    ipc.send(&format!(
        r#"{{"type":"view.layout","id":{id},"x":100,"y":100,"width":400,"height":300}}"#
    ));
    ipc.send(&format!(r#"{{"type":"view.focus","id":{id}}}"#));
    assert_eq!(ipc.expect("view.focused")["id"].as_u64(), Some(id));

    // Wait for the compositor to flag the window. The first ping goes out on
    // the next timer tick and the deadline is several seconds, so this waits
    // generously past both. The window connection is pumped meanwhile so its
    // configure/events are answered (but never the ping).
    let deadline = Instant::now() + Duration::from_secs(12);
    let mut flagged = false;
    while Instant::now() < deadline && !flagged {
        pump(&connection, &mut queue, &mut client);
        for message in ipc.drain(Duration::from_millis(100)) {
            if message["type"] == "view.anr" && message["id"].as_u64() == Some(id) {
                flagged = true;
            }
        }
    }
    assert!(
        flagged,
        "the silent window was never flagged as not responding:\n{}",
        compositor.log()
    );
    println!("ok: a window that does not pong is flagged view.anr");

    // Force quit: the compositor posts the xdg_wm_base unresponsive error and
    // the client's connection dies.
    ipc.send(&format!(
        r#"{{"type":"view.anr_resolve","id":{id},"action":"quit"}}"#
    ));
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut died = false;
    while Instant::now() < deadline && !died {
        died = !pump(&connection, &mut queue, &mut client);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        died,
        "force quit did not disconnect the silent client:\n{}",
        compositor.log()
    );
    println!("ok: force quit disconnects the silent client");
}
