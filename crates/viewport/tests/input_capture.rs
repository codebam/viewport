// SPDX-License-Identifier: GPL-3.0-or-later
//
// Security and discovery seams for the InputCapture backend.

mod common;

use std::{
    collections::HashMap,
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    time::Duration,
};

use common::Compositor;

const BUS_NAME: &str = "org.freedesktop.impl.portal.desktop.viewport";
const OBJECT_PATH: &str = "/org/freedesktop/portal/desktop";
const INTERFACE: &str = "org.freedesktop.impl.portal.InputCapture";
const RESPONSE_FAILED: u32 = 2;
const RESPONSE_CANCELLED: u32 = 1;
const FRONTEND_NAME: &str = "org.freedesktop.portal.Desktop";

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
        let mut address = String::new();
        if BufReader::new(child.stdout.take().expect("piped"))
            .read_line(&mut address)
            .is_err()
            || address.trim().is_empty()
        {
            let _ = child.kill();
            return None;
        }
        Some(Self {
            child,
            address: address.trim().to_owned(),
        })
    }
}

fn compositor_on(bus: &Bus, tag: &str) -> Compositor {
    Compositor::builder(tag)
        .prefix("viewport-input-capture")
        .env("DBUS_SESSION_BUS_ADDRESS", &bus.address)
        .awaiting("portals up", Duration::from_secs(10))
        .start()
}

fn connect(bus: &Bus) -> zbus::blocking::Connection {
    zbus::blocking::connection::Builder::address(bus.address.as_str())
        .expect("bus address")
        .build()
        .expect("private bus connection")
}

#[test]
fn interface_advertises_only_implemented_capabilities() {
    let Some(bus) = Bus::start() else {
        eprintln!("skipped: no dbus-daemon");
        return;
    };
    let _compositor = compositor_on(&bus, "properties");
    let connection = connect(&bus);
    let properties = zbus::blocking::fdo::PropertiesProxy::builder(&connection)
        .destination(BUS_NAME)
        .unwrap()
        .path(OBJECT_PATH)
        .unwrap()
        .build()
        .unwrap();

    let capabilities = properties
        .get(INTERFACE.try_into().unwrap(), "SupportedCapabilities")
        .expect("SupportedCapabilities");
    assert_eq!(u32::try_from(&capabilities).unwrap(), 3);
    let version = properties
        .get(INTERFACE.try_into().unwrap(), "version")
        .expect("version");
    assert_eq!(u32::try_from(&version).unwrap(), 2);

    let introspection = zbus::blocking::Proxy::new(
        &connection,
        BUS_NAME,
        OBJECT_PATH,
        "org.freedesktop.DBus.Introspectable",
    )
    .unwrap();
    let xml: String = introspection.call("Introspect", &()).unwrap();
    for member in [
        "CreateSession",
        "CreateSession2",
        "Start",
        "GetZones",
        "SetPointerBarriers",
        "Enable",
        "Disable",
        "Release",
        "ConnectToEIS",
        "Disabled",
        "Activated",
        "Deactivated",
        "ZonesChanged",
    ] {
        assert!(
            xml.contains(&format!("name=\"{member}\"")),
            "missing {member}"
        );
    }
}

#[test]
fn peer_without_frontend_name_cannot_create_session() {
    let Some(bus) = Bus::start() else {
        eprintln!("skipped: no dbus-daemon");
        return;
    };
    let compositor = compositor_on(&bus, "stranger");
    let connection = connect(&bus);
    let proxy = zbus::blocking::Proxy::new(&connection, BUS_NAME, OBJECT_PATH, INTERFACE).unwrap();
    let request = zvariant::ObjectPath::try_from("/org/example/request/1").unwrap();
    let session = zvariant::ObjectPath::try_from("/org/example/session/1").unwrap();
    let options = HashMap::from([("capabilities", zvariant::Value::from(3u32))]);

    let (response, _): (u32, HashMap<String, zvariant::OwnedValue>) = proxy
        .call(
            "CreateSession",
            &(&request, &session, "org.example.intruder", "", options),
        )
        .expect("CreateSession response");
    assert_eq!(response, RESPONSE_FAILED);
    assert!(compositor.saw("refusing a call from", Duration::from_secs(2)));
}

#[test]
fn start_fails_closed_without_consent_ui() {
    let Some(bus) = Bus::start() else {
        eprintln!("skipped: no dbus-daemon");
        return;
    };
    let _compositor = compositor_on(&bus, "consent");
    let connection = connect(&bus);
    connection.request_name(FRONTEND_NAME).unwrap();
    let proxy = zbus::blocking::Proxy::new(&connection, BUS_NAME, OBJECT_PATH, INTERFACE).unwrap();
    let session = zvariant::ObjectPath::try_from(
        "/org/freedesktop/portal/desktop/session/input_capture_test",
    )
    .unwrap();

    let mut created = false;
    for _ in 0..100 {
        let result: zbus::Result<HashMap<String, zvariant::OwnedValue>> = proxy.call(
            "CreateSession2",
            &(&session, "org.example.capture", no_options()),
        );
        if result.is_ok() {
            created = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(created, "frontend session was never accepted");

    let request = zvariant::ObjectPath::try_from(
        "/org/freedesktop/portal/desktop/request/input_capture_test",
    )
    .unwrap();
    let options = HashMap::from([("capabilities", zvariant::Value::from(3u32))]);
    let (response, _): (u32, HashMap<String, zvariant::OwnedValue>) = proxy
        .call(
            "Start",
            &(&request, &session, "org.example.capture", "", options),
        )
        .unwrap();
    assert_eq!(response, RESPONSE_CANCELLED);
}

fn no_options() -> HashMap<String, zvariant::Value<'static>> {
    HashMap::new()
}
