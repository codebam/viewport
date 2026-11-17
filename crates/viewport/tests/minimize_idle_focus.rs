// SPDX-License-Identifier: GPL-3.0-or-later
//
// A minimized window must survive the shell's next commit.
//
// The defect this pins: minimizing the focused window drops the keyboard to
// nothing, and every commit on the shell's surface runs the idle floor
// (`focus_shell_if_idle`), which used to pick the newest mapped window with
// no minimized filter and hand it the keyboard — and `focus_view` is
// activation, which restores. So the first shell commit after a minimize
// brought the window back. On CI the GTK shell cannot run (bwrap's missing
// loopback) and crash-restarts through the whole suite, which is how the
// view-ipc minimize check failed there while every local run passed; in a
// live session the same floor fires on the page's next redraw after a
// chord-minimize. The rescue layout's focus fallback had the same hole.
//
// The shell here is this same binary, re-exec'd by the compositor through
// VIEWPORT_SHELL_BIN: a client on the shell's own connection that maps a
// toplevel and commits every 50ms, exactly the pressure the crash-loop put
// on the check, but deterministic — the restore used to land within one
// commit of the minimize, so the observation window cannot miss it.
//
// harness = false because one binary plays both parts: with
// VIEWPORT_TEST_IDLE_FOCUS_SHELL set it is the shell, without it the test.

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
    if std::env::var_os("VIEWPORT_TEST_IDLE_FOCUS_SHELL").is_some() {
        shell_mode();
    } else {
        test_mode();
    }
}

/// A buffer and the fd its pool was made from, kept alive together: dropping
/// either proxy or descriptor would take the buffer down while it is attached.
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
    windows: Vec<Window>,
    configured: bool,
    closed: bool,
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

/// A solid-colour shm buffer. The pixels are written with pwrite rather than
/// a mapping: nothing here is captured, and an unmapped fd kept in the client
/// is one less thing for a sanitizer to ask about.
fn paint(shm: &wl_shm::WlShm, w: i32, h: i32, color: u32, qh: &QueueHandle<Client>) -> Paint {
    let name = b"viewport-idle-focus\0";
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

/// Map one toplevel on the connection and wait for its initial configure.
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
    // The initial commit carries no buffer: the configure must be acked
    // before one is attached.
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

/// Read whatever arrived without blocking, dispatch it, flush. False when
/// the connection is gone. `dispatch_pending` alone never reads the socket,
/// so a loop built on it would sit deaf to every configure.
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

/// The shell half: connect on the fd the compositor handed over, map a
/// toplevel, and keep committing — the redraw pressure that used to restore
/// the minimized window, on a clock rather than by luck.
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
    toplevel.set_app_id("idle-focus-shell".to_owned());
    let paint = paint(
        client.shm.as_ref().expect("wl_shm"),
        1920,
        1080,
        0xff202020,
        &qh,
    );
    surface.commit();
    let mut last = Instant::now();
    loop {
        if client.closed || !pump(&connection, &mut queue, &mut client) {
            break;
        }
        // Only once the initial configure has been acked: a commit ahead of
        // the ack is a protocol error, and under load the dispatch can lag.
        if client.configured && last.elapsed() >= Duration::from_millis(50) {
            surface.attach(Some(&paint.buffer), 0, 0);
            surface.commit();
            last = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Throw away everything queued, so the next question is answered fresh
/// rather than by a replay that predates it.
fn discard(ipc: &mut common::Client) {
    while !ipc.drain_lines(Duration::from_millis(50)).is_empty() {}
}

/// The minimized flag of one view, from a fresh view.query.
fn query_minimized(ipc: &mut common::Client, id: u64) -> bool {
    discard(ipc);
    ipc.send(r#"{"type":"view.query"}"#);
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        for message in ipc.drain(Duration::from_millis(200)) {
            if message["type"] == "view.added" && message["id"].as_u64() == Some(id) {
                return message["minimized"].as_bool().expect("minimized flag");
            }
        }
    }
    panic!("view.query never answered for view {id}");
}

/// Wait for the fresh view.added of one app_id, skipping replays of others.
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
    let compositor = Compositor::builder("idle-focus")
        .prefix("viewport-idle-focus")
        .env("VIEWPORT_SHELL_BIN", &exe)
        .env("VIEWPORT_TEST_IDLE_FOCUS_SHELL", "1")
        .env("VIEWPORT_LOG", "viewport=debug")
        .start();
    assert!(
        compositor.saw("shell 0: configuring it to", Duration::from_secs(10)),
        "the fake shell never committed:\n{}",
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

    let mut ipc = compositor.connect();
    make_window(&mut client, &mut queue, "idle-focus-one", 320, 240);
    let one = view_id_for(&mut ipc, "idle-focus-one");
    ipc.send(&format!(
        r#"{{"type":"view.layout","id":{one},"x":10,"y":20,"width":800,"height":600}}"#
    ));
    ipc.send(&format!(r#"{{"type":"view.focus","id":{one}}}"#));
    let focused = ipc.expect("view.focused");
    assert_eq!(
        focused["id"].as_u64(),
        Some(one),
        "the window never took focus"
    );

    // The minimize itself must land before the observation starts.
    ipc.send(&format!(
        r#"{{"type":"view.minimized","id":{one},"minimized":true}}"#
    ));
    assert!(
        query_minimized(&mut ipc, one),
        "the minimize did not stick — a shell commit may have restored it:\n{}",
        compositor.log()
    );

    // Phase one: the shell keeps committing while the keyboard is idle.
    // The restore used to arrive within one commit of the minimize.
    let deadline = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < deadline {
        assert!(
            pump(&connection, &mut queue, &mut client),
            "window connection died"
        );
        assert!(
            query_minimized(&mut ipc, one),
            "a shell commit restored the minimized window:\n{}",
            compositor.log()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    println!("ok: a minimized window survives the shell's commits");

    // Phase two: explicit activation is still restore — the fix must not
    // take that away from the taskbar, the protocol and view.focus.
    ipc.send(&format!(r#"{{"type":"view.focus","id":{one}}}"#));
    let deadline = Instant::now() + Duration::from_secs(2);
    while query_minimized(&mut ipc, one) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !query_minimized(&mut ipc, one),
        "view.focus no longer restores a minimized window"
    );
    ipc.send(&format!(
        r#"{{"type":"view.minimized","id":{one},"minimized":true}}"#
    ));
    assert!(query_minimized(&mut ipc, one));
    println!("ok: explicit activation still restores");

    // Phase three: a second window, minimized before anything ever placed
    // it. The rescue layout fires 2.5s after the map and used to focus the
    // window it rescued — activation again, restore again.
    make_window(&mut client, &mut queue, "idle-focus-two", 320, 240);
    let two = view_id_for(&mut ipc, "idle-focus-two");
    ipc.send(&format!(
        r#"{{"type":"view.minimized","id":{two},"minimized":true}}"#
    ));
    assert!(query_minimized(&mut ipc, two));
    let deadline = Instant::now() + Duration::from_millis(3500);
    while Instant::now() < deadline {
        assert!(
            pump(&connection, &mut queue, &mut client),
            "window connection died"
        );
        assert!(
            query_minimized(&mut ipc, two),
            "the rescue layout restored a minimized window:\n{}",
            compositor.log()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(
        query_minimized(&mut ipc, one),
        "the first window was restored while the second was rescued"
    );
    assert!(
        compositor.saw("falling back to a built-in layout", Duration::from_secs(1)),
        "the rescue never fired, so phase three proved nothing:\n{}",
        compositor.log()
    );
    println!("ok: the rescue layout leaves a minimized window minimized");
}
