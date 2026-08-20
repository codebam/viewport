// SPDX-License-Identifier: GPL-3.0-or-later
//
// Drives the GlobalShortcuts portal over a real bus.
//
// The unit tests in `shortcuts` cover what a trigger turns into and what is
// remembered. What they cannot cover is the seam, and it is the same seam the
// remote-desktop suite exists for: that the interface is on the bus under the
// name the frontend looks for, that a caller which is not the frontend is
// refused, and that a shortcut nobody agreed to is not granted. Getting any of
// them wrong is a compositor that hands out a key from the desk's own keyboard
// to whatever asked for it.
//
// On a bus of this suite's own, never the developer's — see the note at the
// top of `remote_desktop.rs`, which this borrows its harness from — and
// skipped, loudly, where there is no dbus-daemon to start.

mod common;

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use common::Compositor;

const BUS_NAME: &str = "org.freedesktop.impl.portal.desktop.viewport";
const OBJECT_PATH: &str = "/org/freedesktop/portal/desktop";
const INTERFACE: &str = "org.freedesktop.impl.portal.GlobalShortcuts";
const FRONTEND_NAME: &str = "org.freedesktop.portal.Desktop";

const RESPONSE_SUCCESS: u32 = 0;
const RESPONSE_CANCELLED: u32 = 1;
const RESPONSE_FAILED: u32 = 2;

/// A message bus of this test's own, killed on drop.
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

fn compositor_on(bus: &Bus, tag: &str) -> Compositor {
    Compositor::builder(tag)
        .prefix("viewport-shortcuts")
        .env("DBUS_SESSION_BUS_ADDRESS", &bus.address)
        // A state directory of this test's own, so a grant it makes cannot be
        // written into the developer's own record of what they have agreed to.
        .env("XDG_STATE_HOME", "/nonexistent-viewport-test-state")
        .awaiting("portals up", Duration::from_secs(10))
        .start()
}

fn connect(bus: &Bus) -> zbus::blocking::Connection {
    zbus::blocking::connection::Builder::address(bus.address.as_str())
        .expect("a bus address")
        .build()
        .expect("connecting to the test bus")
}

fn proxy(connection: &zbus::blocking::Connection) -> zbus::blocking::Proxy<'static> {
    zbus::blocking::Proxy::new(connection, BUS_NAME, OBJECT_PATH, INTERFACE).expect("a proxy")
}

fn no_options() -> HashMap<String, zvariant::Value<'static>> {
    HashMap::new()
}

/// One shortcut as the frontend passes it on: an id, and the options carrying
/// what it is for and what should fire it.
fn shortcut(id: &str, trigger: &str) -> (String, HashMap<String, zvariant::Value<'static>>) {
    let mut options: HashMap<String, zvariant::Value<'static>> = HashMap::new();
    options.insert(
        "description".to_owned(),
        zvariant::Value::from("talk to people"),
    );
    options.insert(
        "preferred_trigger".to_owned(),
        zvariant::Value::from(trigger.to_owned()),
    );
    (id.to_owned(), options)
}

/// Create a session, retrying while the compositor notices who the frontend
/// is: it follows NameOwnerChanged on a connection of its own, and a call that
/// overtakes that signal is refused for a reason that has already gone away.
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
                &(request, session, "org.example.talker", no_options()),
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

fn paths() -> (zvariant::ObjectPath<'static>, zvariant::ObjectPath<'static>) {
    (
        zvariant::ObjectPath::try_from("/org/freedesktop/portal/desktop/request/1")
            .expect("a path"),
        zvariant::ObjectPath::try_from("/org/freedesktop/portal/desktop/session/1")
            .expect("a path"),
    )
}

/// The interface is on the bus, at the version that describes what is here.
///
/// The frontend finds a backend by the name in `data/portal-share` and asks
/// its version before deciding what it may call. An interface that is merely
/// written is one nothing ever reaches, and that failure is silent: the
/// frontend falls through to another backend, or to none.
#[test]
fn the_interface_is_on_the_bus() {
    let Some(bus) = Bus::start() else {
        eprintln!("skipped: no dbus-daemon to start a private bus with");
        return;
    };
    let _compositor = compositor_on(&bus, "present");

    let connection = connect(&bus);
    let properties = zbus::blocking::fdo::PropertiesProxy::builder(&connection)
        .destination(BUS_NAME)
        .unwrap()
        .path(OBJECT_PATH)
        .unwrap()
        .build()
        .expect("a properties proxy");

    let version = properties
        .get(INTERFACE.try_into().unwrap(), "version")
        .expect("version");
    assert_eq!(u32::try_from(&version).expect("a number"), 1);
}

/// A peer that is not the portal frontend is refused.
///
/// The session bus is reachable by every process in the session, and what this
/// interface hands out is a key that stops reaching whatever the person is
/// typing into. A caller that has not claimed org.freedesktop.portal.Desktop
/// is not the frontend, whatever app id it puts in the call.
#[test]
fn a_stranger_cannot_ask_for_a_shortcut() {
    let Some(bus) = Bus::start() else {
        eprintln!("skipped: no dbus-daemon to start a private bus with");
        return;
    };
    let compositor = compositor_on(&bus, "stranger");

    let connection = connect(&bus);
    let proxy = proxy(&connection);
    let (request, session) = paths();

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

/// A shortcut is refused when there is nobody to ask.
///
/// The headless compositor draws no desktop page, so there is no overlay to
/// put the question in. This is the remote-desktop rule rather than the
/// screen-share one, and for the same reason: a machine that gives away a key
/// from its own keyboard because its user interface is broken has turned a
/// shell bug into a way in.
#[test]
fn a_shortcut_is_refused_with_no_desktop_to_ask_through() {
    let Some(bus) = Bus::start() else {
        eprintln!("skipped: no dbus-daemon to start a private bus with");
        return;
    };
    let compositor = compositor_on(&bus, "noshell");

    let connection = connect(&bus);
    connection
        .request_name(FRONTEND_NAME)
        .expect("claiming the frontend name");
    let proxy = proxy(&connection);
    let (request, session) = paths();
    assert_eq!(create_session(&proxy, &request, &session), RESPONSE_SUCCESS);

    let (response, results): (u32, HashMap<String, zvariant::OwnedValue>) = proxy
        .call(
            "BindShortcuts",
            &(
                &request,
                &session,
                vec![shortcut("talk", "LOGO+SHIFT+s")],
                "",
                no_options(),
            ),
        )
        .expect("BindShortcuts");
    assert_eq!(response, RESPONSE_CANCELLED);
    assert!(
        !results.contains_key("shortcuts"),
        "a refused application was told what it had: {results:?}"
    );
    assert!(
        compositor.saw(
            "refusing to give an application a global shortcut",
            Duration::from_secs(2)
        ),
        "the compositor did not say why it refused; its log:\n{}",
        compositor.log()
    );
}

/// A trigger this keymap cannot read is refused before anybody is asked.
///
/// Nothing here can match it against a key press, so agreeing to it would be
/// agreeing to something that can never happen while telling the application
/// it has a shortcut. The refusal is also the one case that needs no desktop
/// page: there is no question to put on screen.
#[test]
fn a_trigger_this_keymap_cannot_read_is_refused_without_asking() {
    let Some(bus) = Bus::start() else {
        eprintln!("skipped: no dbus-daemon to start a private bus with");
        return;
    };
    let compositor = compositor_on(&bus, "badtrigger");

    let connection = connect(&bus);
    connection
        .request_name(FRONTEND_NAME)
        .expect("claiming the frontend name");
    let proxy = proxy(&connection);
    let (request, session) = paths();
    assert_eq!(create_session(&proxy, &request, &session), RESPONSE_SUCCESS);

    let (response, _): (u32, HashMap<String, zvariant::OwnedValue>) = proxy
        .call(
            "BindShortcuts",
            &(
                &request,
                &session,
                vec![shortcut("talk", "HYPER+NotAKey")],
                "",
                no_options(),
            ),
        )
        .expect("BindShortcuts");
    assert_eq!(response, RESPONSE_CANCELLED);
    assert!(
        compositor.saw("which is not a chord here", Duration::from_secs(2)),
        "the compositor did not say the trigger was unreadable; its log:\n{}",
        compositor.log()
    );
    // And it never got as far as the question, which on a headless compositor
    // would have been refused for the other reason entirely.
    assert!(
        !compositor.log().contains("refusing to give an application"),
        "an unreadable trigger reached the dialogue; its log:\n{}",
        compositor.log()
    );
}

/// A session that was never created holds nothing.
///
/// `ListShortcuts` on a handle the compositor has never seen must answer an
/// empty list rather than somebody else's: the handle is a path the caller
/// chose, and a compositor that matched it loosely would be handing one
/// application the record of another's grants.
#[test]
fn an_unknown_session_holds_nothing() {
    let Some(bus) = Bus::start() else {
        eprintln!("skipped: no dbus-daemon to start a private bus with");
        return;
    };
    let _compositor = compositor_on(&bus, "unknown");

    let connection = connect(&bus);
    connection
        .request_name(FRONTEND_NAME)
        .expect("claiming the frontend name");
    let proxy = proxy(&connection);
    let (request, session) = paths();

    // Retried for the same reason `create_session` is: the compositor has to
    // have noticed who the frontend is before it answers at all.
    let mut answer = (RESPONSE_FAILED, HashMap::new());
    for _ in 0..100 {
        answer = proxy
            .call("ListShortcuts", &(&request, &session))
            .expect("ListShortcuts");
        if answer.0 == RESPONSE_SUCCESS {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let (response, results): (u32, HashMap<String, zvariant::OwnedValue>) = answer;
    assert_eq!(response, RESPONSE_SUCCESS);
    let shortcuts = results.get("shortcuts").expect("a list, even an empty one");
    let shortcuts: Vec<(String, HashMap<String, zvariant::OwnedValue>)> = shortcuts
        .try_to_owned()
        .unwrap()
        .try_into()
        .expect("a list");
    assert!(
        shortcuts.is_empty(),
        "a session nobody created holds {shortcuts:?}"
    );
}
