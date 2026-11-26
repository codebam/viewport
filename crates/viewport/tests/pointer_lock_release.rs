// SPDX-License-Identifier: GPL-3.0-or-later
//
// A pointer lock is the compositor's to take back.
//
// `zwp_pointer_constraints_v1.lock_pointer` asks for the cursor to stop
// moving, and the protocol says the compositor may end that at any time and
// tell the client with `unlocked`. Viewport activates a lock the moment one
// is asked for with the cursor over the surface, and then deliberately keeps
// the pointer's focus there while it holds: a game reads an absolute position
// as aim, so a menu opening over it must not become a motion (`FIXES` #64).
// What that must not become is a capture nothing can end. A window put away
// or unfocused keeps a lock the client asked for while it was on screen, the
// cursor stays hidden, and every click still goes to a window that is not
// there — with no chord on the desk that gives the pointer back, because the
// client that would release it no longer has the keyboard.
//
// So this drives the two ways a locked window stops being the window the
// person is using, and asks for `unlocked` and the pointer's leave:
//
//   * `view.minimized` — the shell's put-away, which drops the keyboard focus;
//   * `view.focus` of another window — the shell's switch, which moves it.
//
// And it pins the capture that must survive: while the lock holds, a motion
// the compositor sends for its own reasons must not reach the client, which
// is the property #64 exists for. A lock that is not released is a lock
// nothing releases; a release that also hands the game aim is the bug this
// would fix it into.
//
// The shell is this same binary re-exec'd through `VIEWPORT_SHELL_BIN`: it
// maps a toplevel so the shell slot is occupied by a client that behaves,
// then sits still — the compositor's focus policy is what these cases turn
// on, not the page.
//
// harness = false because one binary plays both parts: with
// `VIEWPORT_TEST_POINTER_LOCK_SHELL` set it is the shell, without it the test.

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
use wayland_protocols::wp::pointer_constraints::zv1::client::{
    zwp_locked_pointer_v1, zwp_pointer_constraints_v1,
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

fn main() {
    if std::env::var_os("VIEWPORT_TEST_POINTER_LOCK_SHELL").is_some() {
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
    pointer: Option<wl_pointer::WlPointer>,
    constraints: Option<zwp_pointer_constraints_v1::ZwpPointerConstraintsV1>,
    lock: Option<zwp_locked_pointer_v1::ZwpLockedPointerV1>,
    windows: Vec<Window>,
    configured: Vec<bool>,
    closed: bool,
    entered: bool,
    /// The surface the pointer is over, so the lock names the right one.
    entered_surface: Option<wl_surface::WlSurface>,
    pointer_pos: Option<(f64, f64)>,
    locked: bool,
    unlocked: bool,
    motions_while_locked: u32,
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
            "zwp_pointer_constraints_v1" => {
                state.constraints = Some(registry.bind(name, version.min(1), qh, ()));
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
delegate_noop!(Client: ignore zwp_pointer_constraints_v1::ZwpPointerConstraintsV1);

impl Dispatch<zwp_locked_pointer_v1::ZwpLockedPointerV1, ()> for Client {
    fn event(
        state: &mut Self,
        _: &zwp_locked_pointer_v1::ZwpLockedPointerV1,
        event: zwp_locked_pointer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_locked_pointer_v1::Event::Locked => {
                state.locked = true;
                state.unlocked = false;
            }
            zwp_locked_pointer_v1::Event::Unlocked => {
                state.unlocked = true;
                state.locked = false;
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for Client {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                serial,
                surface,
                surface_x,
                surface_y,
            } => {
                state.entered = true;
                state.entered_surface = Some(surface.clone());
                state.pointer_pos = Some((surface_x, surface_y));
                // A game asks for the lock when it has the pointer, which is
                // this moment. Persistent: `unlocked` is a release the
                // compositor may take back, not the end of the request.
                if let (Some(constraints), Some(pointer), None) = (
                    state.constraints.as_ref(),
                    state.pointer.as_ref(),
                    state.lock.as_ref(),
                ) {
                    let _ = serial;
                    state.lock = Some(constraints.lock_pointer(
                        &surface,
                        pointer,
                        None,
                        zwp_pointer_constraints_v1::Lifetime::Persistent,
                        qh,
                        (),
                    ));
                }
            }
            wl_pointer::Event::Motion { .. } => {
                state.pointer_pos = Some((0.0, 0.0));
                if state.locked {
                    state.motions_while_locked += 1;
                }
            }
            wl_pointer::Event::Leave { .. } => {
                state.entered = false;
                state.entered_surface = None;
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

impl Dispatch<xdg_surface::XdgSurface, usize> for Client {
    fn event(
        state: &mut Self,
        surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            surface.ack_configure(serial);
            if let Some(configured) = state.configured.get_mut(*index) {
                *configured = true;
            }
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, usize> for Client {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_toplevel::Event::Close = event {
            state.closed = true;
        }
    }
}

fn paint(shm: &wl_shm::WlShm, w: i32, h: i32, color: u32, qh: &QueueHandle<Client>) -> Paint {
    let name = b"viewport-pointer-lock\0";
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
    let index = client.windows.len();
    let surface = client
        .compositor
        .as_ref()
        .expect("wl_compositor")
        .create_surface(&qh, ());
    let xdg = client
        .wm_base
        .as_ref()
        .expect("xdg_wm_base")
        .get_xdg_surface(&surface, &qh, index);
    let toplevel = xdg.get_toplevel(&qh, index);
    toplevel.set_app_id(app_id.to_owned());
    toplevel.set_title(app_id.to_owned());
    let paint = paint(client.shm.as_ref().expect("wl_shm"), w, h, 0xff30a050, &qh);
    surface.commit();
    client.configured.push(false);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !client.configured[index] && Instant::now() < deadline {
        queue.roundtrip(client).expect("initial configure");
    }
    assert!(client.configured[index], "{app_id} was never configured");
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

/// Pump for a while, returning as soon as `done` says so.
fn until(
    connection: &Connection,
    queue: &mut EventQueue<Client>,
    client: &mut Client,
    within: Duration,
    done: impl Fn(&Client) -> bool,
) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if done(client) {
            return true;
        }
        assert!(
            pump(connection, queue, client),
            "the client's connection died"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    done(client)
}

fn shell_mode() {
    let connection = Connection::connect_to_env().expect("the shell's WAYLAND_SOCKET");
    let mut queue = connection.new_event_queue();
    let qh = queue.handle();
    connection.display().get_registry(&qh, ());
    let mut client = Client::default();
    queue.roundtrip(&mut client).expect("globals");
    make_window(&mut client, &mut queue, "pointer-lock-shell", 1920, 1080);
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
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn test_mode() {
    let exe = std::env::current_exe().expect("current exe");
    let compositor = Compositor::builder("pointer-lock")
        .prefix("viewport-pointer-lock")
        .env("VIEWPORT_SHELL_BIN", &exe)
        .env("VIEWPORT_TEST_POINTER_LOCK_SHELL", "1")
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
    assert!(
        client.constraints.is_some(),
        "zwp_pointer_constraints_v1 was never offered to a native client:\n{}",
        compositor.log()
    );
    let mut ipc = compositor.connect();

    // The window sits at (100, 100) sized 400x300: surface-local (0..400,
    // 0..300), global (100..500, 100..400). Its centre is global (300, 250).
    make_window(&mut client, &mut queue, "pointer-lock-window", 400, 300);
    let id = view_id_for(&mut ipc, "pointer-lock-window");
    ipc.send(&format!(
        r#"{{"type":"view.layout","id":{id},"x":100,"y":100,"width":400,"height":300}}"#
    ));
    ipc.send(&format!(r#"{{"type":"view.focus","id":{id}}}"#));
    assert_eq!(ipc.expect("view.focused")["id"].as_u64(), Some(id));

    // The pointer goes in, and the client asks for the lock on the enter.
    move_pointer(&mut ipc, &connection, &mut queue, &mut client, 300.0, 250.0);
    assert!(client.entered, "the pointer never entered the window");
    assert!(
        until(
            &connection,
            &mut queue,
            &mut client,
            Duration::from_secs(5),
            |c| c.locked
        ),
        "the compositor never granted the lock:\n{}",
        compositor.log()
    );
    println!("ok: a client's lock is granted while the pointer is over it");

    // A motion the compositor sends for its own reasons — here a layout that
    // moves nothing — must not reach a captured client as a position. This is
    // the property `FIXES` #64 protects.
    let motions = client.motions_while_locked;
    ipc.send(&format!(
        r#"{{"type":"view.layout","id":{id},"x":100,"y":100,"width":400,"height":300}}"#
    ));
    move_pointer(&mut ipc, &connection, &mut queue, &mut client, 300.0, 250.0);
    assert!(
        client.entered,
        "a captured pointer was handed a leave by an unrelated relayout"
    );
    assert_eq!(
        client.motions_while_locked, motions,
        "a captured pointer was handed a position by an unrelated relayout"
    );
    println!("ok: a held capture survives the shell's own relayout, unmoved");

    // The first way a locked window stops being the one in use: the shell puts
    // it away. A minimized window is off the layout the page draws, and the
    // page hides it (`geometry.js`), which is what takes it out of the space
    // here; the keyboard focus goes with it. The capture must go too.
    ipc.send(&format!(
        r#"{{"type":"view.minimized","id":{id},"minimized":true}}"#
    ));
    ipc.send(&format!(
        r#"{{"type":"view.visible","id":{id},"visible":false}}"#
    ));
    assert!(
        until(
            &connection,
            &mut queue,
            &mut client,
            Duration::from_secs(5),
            |c| { c.unlocked }
        ),
        "minimizing a captured window left the lock held — the cursor stays hidden \
         and every click still goes to a window that is not on screen:\n{}",
        compositor.log()
    );
    assert!(
        until(
            &connection,
            &mut queue,
            &mut client,
            Duration::from_secs(5),
            |c| { !c.entered }
        ),
        "the minimized window kept the pointer's focus:\n{}",
        compositor.log()
    );
    println!("ok: minimizing a captured window releases it and the pointer leaves");

    // The minimize told the shell the focus was gone; take that out of the
    // queue before waiting for the next one, which is a different message.
    let _ = ipc.drain(Duration::from_millis(300));

    // The other way: the shell focuses a different window. The lock was asked
    // for while the cursor was here, and the keyboard is now somewhere else.
    ipc.send(&format!(
        r#"{{"type":"view.minimized","id":{id},"minimized":false}}"#
    ));
    ipc.send(&format!(
        r#"{{"type":"view.visible","id":{id},"visible":true}}"#
    ));
    ipc.send(&format!(r#"{{"type":"view.focus","id":{id}}}"#));
    assert_eq!(ipc.expect("view.focused")["id"].as_u64(), Some(id));
    move_pointer(&mut ipc, &connection, &mut queue, &mut client, 300.0, 250.0);
    assert!(
        until(
            &connection,
            &mut queue,
            &mut client,
            Duration::from_secs(5),
            |c| c.locked
        ),
        "the lock was not granted again after the window came back:\n{}",
        compositor.log()
    );
    make_window(&mut client, &mut queue, "pointer-lock-other", 400, 300);
    let other = view_id_for(&mut ipc, "pointer-lock-other");
    ipc.send(&format!(
        r#"{{"type":"view.layout","id":{other},"x":600,"y":100,"width":400,"height":300}}"#
    ));
    ipc.send(&format!(r#"{{"type":"view.focus","id":{other}}}"#));
    assert_eq!(ipc.expect("view.focused")["id"].as_u64(), Some(other));
    assert!(
        until(
            &connection,
            &mut queue,
            &mut client,
            Duration::from_secs(5),
            |c| { c.unlocked }
        ),
        "focusing another window left the old one's capture held — the keyboard is \
         somewhere the pointer is not:\n{}",
        compositor.log()
    );
    println!("ok: focusing another window releases the previous capture");
}
