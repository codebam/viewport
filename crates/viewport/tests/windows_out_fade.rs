// SPDX-License-Identifier: GPL-3.0-or-later
//
// windowsOut: a closing window is photographed and faded, not vanished.
//
// The compositor keeps the last frame it drew of a closing window and fades
// that out under the windowsOut motion, because the client destroys its
// surface before the shell hears of the close — there is nothing left for the
// page to animate. This watches the compositor's half of that: a close with
// the motion on keeps a ghost (the log says so, and the window's removal is
// deferred to the render pass that photographs it), and a close with the
// motion off removes at once with no ghost.
//
// The fade's arithmetic — alpha, zoom, slide, the curve and the spring, the
// inheritance from `windows` — is unit-tested in `crate::ghost`; this is the
// lifecycle around it, driven by a real client destroying its own toplevel.
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
    wl_buffer, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

fn main() {
    if std::env::var_os("VIEWPORT_TEST_OUT_SHELL").is_some() {
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
    /// The toplevels this client maps, kept so the test can destroy one to
    /// close its window the way an application does.
    toplevels: Vec<xdg_toplevel::XdgToplevel>,
    xdg_surfaces: Vec<xdg_surface::XdgSurface>,
    surfaces: Vec<wl_surface::WlSurface>,
    paints: Vec<Paint>,
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

fn paint(shm: &wl_shm::WlShm, w: i32, h: i32, color: u32, qh: &QueueHandle<Client>) -> Paint {
    let name = b"viewport-out\0";
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
    let paint = paint(client.shm.as_ref().expect("wl_shm"), w, h, 0xffa05030, &qh);
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
    client.toplevels.push(toplevel);
    client.xdg_surfaces.push(xdg);
    client.surfaces.push(surface);
    client.paints.push(paint);
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
    make_window(&mut client, &mut queue, "out-shell", 1920, 1080);
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

/// Map a window, place it, focus it, and hand back its id.
fn place_window(
    ipc: &mut common::Client,
    client: &mut Client,
    queue: &mut EventQueue<Client>,
    app_id: &str,
) -> u64 {
    make_window(client, queue, app_id, 400, 300);
    let id = view_id_for(ipc, app_id);
    ipc.send(&format!(
        r#"{{"type":"view.layout","id":{id},"x":100,"y":100,"width":400,"height":300}}"#
    ));
    ipc.send(&format!(r#"{{"type":"view.focus","id":{id}}}"#));
    assert_eq!(ipc.expect("view.focused")["id"].as_u64(), Some(id));
    // Let the placement's configure arrive and be acked before the test closes
    // the window: destroying a toplevel with a configure still in flight acks a
    // serial the compositor has already moved past.
    let deadline = Instant::now() + Duration::from_millis(300);
    while Instant::now() < deadline {
        let _ = queue.roundtrip(client);
        std::thread::sleep(Duration::from_millis(20));
    }
    id
}

/// How many ghosts the compositor has said it kept.
fn ghost_count(compositor: &Compositor) -> usize {
    compositor.log().matches("windowsOut: view").count()
}

fn test_mode() {
    // Two independent compositors, one window each: a close with the motion on
    // keeps a ghost to fade, and a close with it off removes at once. Separate
    // sessions keep a destroyed-but-lingering surface from one case out of the
    // next's configure serials.
    run_case(
        "fade",
        r#"{"motion":{"windows_out":{"style":"fade","duration":300}}}"#,
        true,
    );
    run_case("instant", r#"{"motion":{"enabled":false}}"#, false);
}

fn run_case(tag: &str, config_json: &str, expect_ghost: bool) {
    let exe = std::env::current_exe().expect("current exe");
    let dir = std::env::temp_dir().join(format!("viewport-out-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("test dir");
    let config = dir.join("config.json");
    std::fs::write(&config, config_json).expect("write config");

    let compositor = Compositor::builder(tag)
        .prefix("viewport-windows-out")
        .owning(dir.clone())
        .arg("--config")
        .arg(&config)
        .env("VIEWPORT_SHELL_BIN", &exe)
        .env("VIEWPORT_TEST_OUT_SHELL", "1")
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
    let mut ipc = compositor.connect();

    // The client destroys its toplevel the way an application closes; the
    // surface lives on a moment so the render pass can photograph it. The shell
    // is told the window is gone either way.
    let id = place_window(&mut ipc, &mut client, &mut queue, "out-window");
    let before = ghost_count(&compositor);
    client.toplevels[0].destroy();
    queue.roundtrip(&mut client).expect("close roundtrip");
    let removed = ipc.expect("view.removed");
    assert_eq!(
        removed["id"].as_u64(),
        Some(id),
        "the close never reached the shell"
    );

    if expect_ghost {
        assert!(
            compositor.saw("windowsOut: view", Duration::from_secs(5)),
            "a close with the motion on kept no ghost:\n{}",
            compositor.log()
        );
        assert!(
            ghost_count(&compositor) > before,
            "the ghost count did not rise"
        );
        println!("ok: a close with windowsOut on keeps a frame to fade");

        // The ghost is spent after its duration; the view is gone from the
        // registry once the render pass has photographed and removed it.
        std::thread::sleep(Duration::from_millis(450));
        ipc.send(r#"{"type":"view.query"}"#);
        let messages = ipc.drain(Duration::from_millis(500));
        let still_there = messages
            .iter()
            .any(|m| m["type"] == "view.added" && m["id"].as_u64() == Some(id));
        assert!(!still_there, "the closed window was never removed");
        println!("ok: the ghost is spent and the window removed after its fade");
    } else {
        // Give a frame a chance to run, then confirm no ghost was kept.
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(
            ghost_count(&compositor),
            before,
            "a close with the motion off kept a ghost:\n{}",
            compositor.log()
        );
        println!("ok: a close with windowsOut off removes at once, no ghost");
    }
}
