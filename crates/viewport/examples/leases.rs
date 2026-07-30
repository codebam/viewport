// SPDX-License-Identifier: GPL-3.0-or-later
//
// What a VR runtime sees: the connectors offered for lease, over
// `wp-drm-lease-v1`.
//
//   cargo run -p viewport --example leases
//
// A machine with no head-mounted display offers nothing, and prints a device
// with no connectors — which is the correct answer rather than a failure. What
// it does prove is that the global is there, that the compositor hands over a
// DRM fd, and that the connector list is terminated with `done`; the parts a
// headset would then lease.

use std::sync::{Arc, Mutex};

use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle,
    protocol::wl_registry::{self, WlRegistry},
};
use wayland_protocols::wp::drm_lease::v1::client::{
    wp_drm_lease_connector_v1::{self, WpDrmLeaseConnectorV1},
    wp_drm_lease_device_v1::{self, WpDrmLeaseDeviceV1},
};

#[derive(Default)]
struct Seen {
    /// name, description, by the object they arrived on
    connectors: Vec<(u32, String, String)>,
    fd: bool,
    done: bool,
}

struct App {
    seen: Arc<Mutex<Seen>>,
}

impl Dispatch<WlRegistry, ()> for App {
    fn event(
        _state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name, interface, ..
        } = event
        {
            if interface == "wp_drm_lease_device_v1" {
                registry.bind::<WpDrmLeaseDeviceV1, _, _>(name, 1, qh, ());
            }
        }
    }
}

impl Dispatch<WpDrmLeaseDeviceV1, ()> for App {
    fn event(
        state: &mut Self,
        _: &WpDrmLeaseDeviceV1,
        event: wp_drm_lease_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            // The fd is the whole point: a lease is granted on it, and a
            // compositor that offers the protocol without handing one over has
            // nothing a client can use.
            wp_drm_lease_device_v1::Event::DrmFd { .. } => state.seen.lock().unwrap().fd = true,
            wp_drm_lease_device_v1::Event::Done => state.seen.lock().unwrap().done = true,
            _ => {}
        }
    }

    wayland_client::event_created_child!(App, WpDrmLeaseDeviceV1, [
        wp_drm_lease_device_v1::EVT_CONNECTOR_OPCODE => (WpDrmLeaseConnectorV1, ()),
    ]);
}

impl Dispatch<WpDrmLeaseConnectorV1, ()> for App {
    fn event(
        state: &mut Self,
        connector: &WpDrmLeaseConnectorV1,
        event: wp_drm_lease_connector_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let object = connector.id().protocol_id();
        let mut seen = state.seen.lock().unwrap();
        if !seen.connectors.iter().any(|c| c.0 == object) {
            seen.connectors
                .push((object, String::new(), String::new()));
        }
        let entry = seen
            .connectors
            .iter_mut()
            .find(|c| c.0 == object)
            .unwrap();
        match event {
            wp_drm_lease_connector_v1::Event::Name { name } => entry.1 = name,
            wp_drm_lease_connector_v1::Event::Description { description } => entry.2 = description,
            _ => {}
        }
    }
}

fn main() {
    let conn = Connection::connect_to_env().expect("no compositor");
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());

    let mut app = App {
        seen: Arc::new(Mutex::new(Seen::default())),
    };
    queue.roundtrip(&mut app).expect("registry");
    for _ in 0..3 {
        queue.roundtrip(&mut app).expect("roundtrip");
        if app.seen.lock().unwrap().done {
            break;
        }
    }

    let seen = app.seen.lock().unwrap();
    if !seen.fd && !seen.done && seen.connectors.is_empty() {
        eprintln!("this compositor has no wp_drm_lease_device_v1");
        std::process::exit(1);
    }
    println!(
        "drm fd offered: {}, list terminated: {}, {} connector(s) for lease",
        seen.fd,
        seen.done,
        seen.connectors.len()
    );
    for (_, name, description) in seen.connectors.iter() {
        println!("  {name}  {description}");
    }
}
