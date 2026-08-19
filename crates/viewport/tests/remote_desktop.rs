// SPDX-License-Identifier: GPL-3.0-or-later
//
// Drives the RemoteDesktop portal over a real bus.
//
// What the unit tests in `screencast::remote` cannot cover is the seam: that
// the interface is actually on the bus under the name the frontend looks for,
// that a call from a peer which is not the frontend is refused, and that a
// session nobody agreed to is not granted anything. All three are properties
// of the wiring rather than of any function, and getting any of them wrong is
// a compositor that hands control of itself to whatever asked.
//
// On a bus of this suite's own, never the developer's. The compositor claims
// org.freedesktop.impl.portal.desktop.viewport when it starts, and a test that
// let it do so on the live session bus would be a test that fights the desktop
// the person running it is sitting in front of. `dbus-daemon --session` is one
// process and one socket in /tmp, and it goes when the test does.
//
// Skipped, loudly, where there is no dbus-daemon to start. The Rust dev shell
// does not carry one — it carries what `cargo test --workspace` needs, which
// is deliberately small enough to run on an unassisted CI runner — so this
// suite is a thing that runs on a workstation and says why it did not
// elsewhere, rather than a red build on a machine that was never going to have
// a bus.

mod common;

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use common::Compositor;

/// Where the compositor serves every portal interface it implements.
const BUS_NAME: &str = "org.freedesktop.impl.portal.desktop.viewport";
const OBJECT_PATH: &str = "/org/freedesktop/portal/desktop";
const INTERFACE: &str = "org.freedesktop.impl.portal.RemoteDesktop";

/// The name a caller has to own for the compositor to answer it at all.
const FRONTEND_NAME: &str = "org.freedesktop.portal.Desktop";

/// The portal's response codes.
const RESPONSE_SUCCESS: u32 = 0;
const RESPONSE_CANCELLED: u32 = 1;
const RESPONSE_FAILED: u32 = 2;

/// Every device the interface knows about: keyboard, pointer, touchscreen.
const ALL_DEVICES: u32 = 7;

/// A message bus of this test's own, and the address to reach it on.
///
/// Killed on drop, which is what keeps a failed test from leaving a daemon
/// behind: these run in one process and a panic unwinds through here.
struct Bus {
    child: Child,
    address: String,
}

impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Bus {
    /// Start one, or say why not.
    ///
    /// `None` rather than a panic when there is no dbus-daemon on the path:
    /// see the note at the top of this file. The address is the first line the
    /// daemon prints, which it does as soon as it is listening — so reading it
    /// is also the wait for the socket to exist, and there is no sleep here.
    fn start() -> Option<Self> {
        let mut child = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let stdout = child.stdout.take().expect("piped");
        let mut address = String::new();
        if BufReader::new(stdout).read_line(&mut address).is_err() || address.trim().is_empty() {
            let _ = child.kill();
            return None;
        }
        Some(Bus {
            child,
            address: address.trim().to_owned(),
        })
    }
}

/// A compositor on that bus, with its portals up.
///
/// Waited for by the line the portal code logs rather than by the control
/// socket: the socket exists well before the bus name is claimed, and a call
/// that arrives first is refused for a reason that has nothing to do with what
/// is being tested.
fn compositor_on(bus: &Bus, tag: &str) -> Compositor {
    Compositor::builder(tag)
        .prefix("viewport-remote")
        .env("DBUS_SESSION_BUS_ADDRESS", &bus.address)
        .awaiting("portals up", Duration::from_secs(10))
        .start()
}

/// A connection to that bus.
fn connect(bus: &Bus) -> zbus::blocking::Connection {
    zbus::blocking::connection::Builder::address(bus.address.as_str())
        .expect("a bus address")
        .build()
        .expect("connecting to the test bus")
}

/// A proxy for the interface under test.
fn proxy(connection: &zbus::blocking::Connection) -> zbus::blocking::Proxy<'static> {
    zbus::blocking::Proxy::new(connection, BUS_NAME, OBJECT_PATH, INTERFACE).expect("a proxy")
}

/// An empty options dictionary, which most of these calls take.
fn no_options() -> HashMap<String, zvariant::Value<'static>> {
    HashMap::new()
}

/// Create a session, retrying while the compositor catches up.
///
/// Owning the frontend name and the compositor having noticed are two
/// different moments: it follows NameOwnerChanged on a connection of its own,
/// and a call that overtakes that signal is refused for a reason that has gone
/// away by the time the refusal is read.
fn create_session(
    proxy: &zbus::blocking::Proxy<'_>,
    request: &zvariant::ObjectPath<'_>,
    session: &zvariant::ObjectPath<'_>,
) -> u32 {
    let mut response = RESPONSE_FAILED;
    for _ in 0..100 {
        let (answer, _): (u32, HashMap<String, zvariant::OwnedValue>) = proxy
            .call(
                "CreateSession",
                &(request, session, "org.example.remote", no_options()),
            )
            .expect("CreateSession");
        response = answer;
        if response == RESPONSE_SUCCESS {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    response
}

/// The interface is on the bus, and says what it can be driven with.
///
/// The one thing no unit test can check: the object is served at the path the
/// frontend looks at, under the name in `data/portal-share`. An interface that
/// is merely written is one the frontend never finds — and the way that fails
/// is silent, because the frontend simply falls through to another backend.
#[test]
fn the_interface_is_on_the_bus() {
    let Some(bus) = Bus::start() else {
        eprintln!("skipped: no dbus-daemon to start a private bus with");
        return;
    };
    // Bound so it lives to the end of the test; nothing here reads its log.
    let _compositor = compositor_on(&bus, "present");

    let connection = connect(&bus);
    let properties = zbus::blocking::fdo::PropertiesProxy::builder(&connection)
        .destination(BUS_NAME)
        .unwrap()
        .path(OBJECT_PATH)
        .unwrap()
        .build()
        .expect("a properties proxy");

    let devices = properties
        .get(INTERFACE.try_into().unwrap(), "AvailableDeviceTypes")
        .expect("AvailableDeviceTypes");
    assert_eq!(u32::try_from(&devices).expect("a number"), ALL_DEVICES);

    // Version one, deliberately: two is the version that promises
    // ConnectToEIS, and there is no EIS server here. Claiming it would send
    // applications down a path that answers with an error.
    let version = properties
        .get(INTERFACE.try_into().unwrap(), "version")
        .expect("version");
    assert_eq!(u32::try_from(&version).expect("a number"), 1);
}

/// A peer that is not the portal frontend is refused.
///
/// The session bus is reachable by every process in the session, and the calls
/// on this interface hand over the keyboard. A caller that has not claimed
/// org.freedesktop.portal.Desktop is not the frontend, whatever it says it is.
#[test]
fn a_stranger_cannot_start_a_session() {
    let Some(bus) = Bus::start() else {
        eprintln!("skipped: no dbus-daemon to start a private bus with");
        return;
    };
    let compositor = compositor_on(&bus, "stranger");

    let connection = connect(&bus);
    let proxy = proxy(&connection);
    let request = zvariant::ObjectPath::try_from("/org/example/request/1").unwrap();
    let session = zvariant::ObjectPath::try_from("/org/example/session/1").unwrap();

    // Not retried, unlike the frontend's own call: this one is expected to
    // fail and retrying it would only be waiting for it to keep failing.
    let (response, _): (u32, HashMap<String, zvariant::OwnedValue>) = proxy
        .call(
            "CreateSession",
            &(&request, &session, "org.example.intruder", no_options()),
        )
        .expect("CreateSession");
    assert_eq!(response, RESPONSE_FAILED);
    assert!(
        compositor.saw("refusing a call from", Duration::from_secs(2)),
        "the compositor said nothing about refusing a stranger; its log:\n{}",
        compositor.log()
    );
}

/// A session is refused when there is nobody to ask.
///
/// The headless compositor has no desktop page, so there is no overlay to put
/// the question in. The screen-share path falls back to sharing the focused
/// window in that case; this one must not — a machine that grants control of
/// itself because its own shell is missing has turned a broken desktop into a
/// way in. Cancelled rather than failed, because from the application's side
/// it is indistinguishable from a person saying no.
#[test]
fn a_session_is_refused_with_no_desktop_to_ask_through() {
    let Some(bus) = Bus::start() else {
        eprintln!("skipped: no dbus-daemon to start a private bus with");
        return;
    };
    let compositor = compositor_on(&bus, "noshell");

    let connection = connect(&bus);
    // The compositor answers nothing to a peer that does not own this.
    connection
        .request_name(FRONTEND_NAME)
        .expect("claiming the frontend name");
    let proxy = proxy(&connection);

    let request = zvariant::ObjectPath::try_from("/org/freedesktop/portal/desktop/request/1")
        .expect("a path");
    let session = zvariant::ObjectPath::try_from("/org/freedesktop/portal/desktop/session/1")
        .expect("a path");
    assert_eq!(create_session(&proxy, &request, &session), RESPONSE_SUCCESS);

    let mut devices: HashMap<String, zvariant::Value<'_>> = HashMap::new();
    devices.insert("types".to_owned(), zvariant::Value::from(ALL_DEVICES));
    let (response, _): (u32, HashMap<String, zvariant::OwnedValue>) = proxy
        .call(
            "SelectDevices",
            &(&request, &session, "org.example.remote", devices),
        )
        .expect("SelectDevices");
    assert_eq!(response, RESPONSE_SUCCESS);

    let (response, results): (u32, HashMap<String, zvariant::OwnedValue>) = proxy
        .call(
            "Start",
            &(&request, &session, "org.example.remote", "", no_options()),
        )
        .expect("Start");
    assert_eq!(response, RESPONSE_CANCELLED);
    assert!(
        !results.contains_key("devices"),
        "a refused session was told what it could drive: {results:?}"
    );
    assert!(
        compositor.saw(
            "refusing to let an application drive",
            Duration::from_secs(2)
        ),
        "the compositor did not say why it refused; its log:\n{}",
        compositor.log()
    );
}

/// An application that asked for no devices is refused before anyone is asked.
///
/// A grant of nothing is not a question worth putting on screen: there is no
/// sentence that describes it and no answer that changes anything. It is also
/// what a client that skipped SelectDevices looks like.
#[test]
fn a_session_that_wants_no_devices_is_refused() {
    let Some(bus) = Bus::start() else {
        eprintln!("skipped: no dbus-daemon to start a private bus with");
        return;
    };
    let compositor = compositor_on(&bus, "nodevices");

    let connection = connect(&bus);
    connection
        .request_name(FRONTEND_NAME)
        .expect("claiming the frontend name");
    let proxy = proxy(&connection);

    let request = zvariant::ObjectPath::try_from("/org/freedesktop/portal/desktop/request/2")
        .expect("a path");
    let session = zvariant::ObjectPath::try_from("/org/freedesktop/portal/desktop/session/2")
        .expect("a path");
    assert_eq!(create_session(&proxy, &request, &session), RESPONSE_SUCCESS);

    let mut devices: HashMap<String, zvariant::Value<'_>> = HashMap::new();
    devices.insert("types".to_owned(), zvariant::Value::from(0u32));
    let (response, _): (u32, HashMap<String, zvariant::OwnedValue>) = proxy
        .call(
            "SelectDevices",
            &(&request, &session, "org.example.remote", devices),
        )
        .expect("SelectDevices");
    assert_eq!(response, RESPONSE_SUCCESS);

    let (response, _): (u32, HashMap<String, zvariant::OwnedValue>) = proxy
        .call(
            "Start",
            &(&request, &session, "org.example.remote", "", no_options()),
        )
        .expect("Start");
    assert_eq!(response, RESPONSE_CANCELLED);
    assert!(
        compositor.saw("which asked for no devices", Duration::from_secs(2)),
        "the compositor did not say why it refused; its log:\n{}",
        compositor.log()
    );
}

/// Input from a session that was never granted anything goes nowhere.
///
/// The Notify calls have no return value — the interface defines them one-way
/// — so the only refusal available is to drop the event, and the only way to
/// see that it happened is that the compositor is still there afterwards and
/// says so. What is being ruled out is an application that skips Start
/// entirely and simply starts typing.
#[test]
fn an_ungranted_session_injects_nothing() {
    let Some(bus) = Bus::start() else {
        eprintln!("skipped: no dbus-daemon to start a private bus with");
        return;
    };
    let compositor = compositor_on(&bus, "ungranted");

    let connection = connect(&bus);
    connection
        .request_name(FRONTEND_NAME)
        .expect("claiming the frontend name");
    let proxy = proxy(&connection);

    let request = zvariant::ObjectPath::try_from("/org/freedesktop/portal/desktop/request/3")
        .expect("a path");
    let session = zvariant::ObjectPath::try_from("/org/freedesktop/portal/desktop/session/3")
        .expect("a path");
    assert_eq!(create_session(&proxy, &request, &session), RESPONSE_SUCCESS);

    // No Start, so nothing was granted. One of each kind, because the grant is
    // per device and a hole in one of the three is a hole.
    proxy
        .call_noreply(
            "NotifyKeyboardKeycode",
            &(&session, no_options(), 30i32, 1u32),
        )
        .expect("NotifyKeyboardKeycode");
    proxy
        .call_noreply("NotifyPointerMotion", &(&session, no_options(), 10.0, 10.0))
        .expect("NotifyPointerMotion");
    proxy
        .call_noreply("NotifyTouchUp", &(&session, no_options(), 0u32))
        .expect("NotifyTouchUp");

    // Still answering afterwards, which is the other half of what is being
    // checked: an injection path that panicked on the bus thread would take
    // the settings portal down with it and leave nothing in the log to say so.
    let properties = zbus::blocking::fdo::PropertiesProxy::builder(&connection)
        .destination(BUS_NAME)
        .unwrap()
        .path(OBJECT_PATH)
        .unwrap()
        .build()
        .expect("a properties proxy");
    let version = properties
        .get(INTERFACE.try_into().unwrap(), "version")
        .expect("the portal is still answering");
    assert_eq!(u32::try_from(&version).expect("a number"), 1);

    assert!(
        !compositor.log().contains("panicked"),
        "the compositor panicked; its log:\n{}",
        compositor.log()
    );
}
