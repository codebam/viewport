// SPDX-License-Identifier: GPL-3.0-or-later
//
// What an external bar sees: the workspaces, over `ext-workspace-v1`.
//
// `wayland-info` lists the global and does not bind it, so it cannot answer
// whether the compositor publishes anything through it. This binds it, prints
// what arrives, and exits on the first `done` — which is the protocol's own
// statement that it has finished describing the world.
//
//   cargo run -p viewport --example workspaces
//   cargo run -p viewport --example workspaces -- --activate <id>
//
// With `--activate` it asks for a workspace and commits, which is the other
// half: the request goes to the compositor, out to the shell as
// `workspace.request`, and the shell is what makes it true.

use std::sync::{Arc, Mutex};

use wayland_client::{
    protocol::wl_registry::{self, WlRegistry},
    Connection, Dispatch, Proxy, QueueHandle,
};
use wayland_protocols::ext::workspace::v1::client::{
    ext_workspace_group_handle_v1::{self, ExtWorkspaceGroupHandleV1},
    ext_workspace_handle_v1::{self, ExtWorkspaceHandleV1},
    ext_workspace_manager_v1::{self, ExtWorkspaceManagerV1},
};

/// Everything learned about one workspace, and the object it came on.
#[derive(Clone)]
struct Known {
    handle: ExtWorkspaceHandleV1,
    id: String,
    name: String,
    state: String,
}

#[derive(Default)]
struct Seen {
    workspaces: Vec<Known>,
    groups: Vec<u32>,
    done: bool,
}

struct App {
    manager: Option<ExtWorkspaceManagerV1>,
    seen: Arc<Mutex<Seen>>,
    activate: Option<String>,
}

impl Dispatch<WlRegistry, ()> for App {
    fn event(
        state: &mut Self,
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
            if interface == "ext_workspace_manager_v1" {
                state.manager = Some(registry.bind::<ExtWorkspaceManagerV1, _, _>(name, 1, qh, ()));
            }
        }
    }
}

impl Dispatch<ExtWorkspaceManagerV1, ()> for App {
    fn event(
        state: &mut Self,
        _: &ExtWorkspaceManagerV1,
        event: ext_workspace_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            // Counted here rather than on `output_enter`: a group exists as
            // soon as the manager says so, and the output only arrives for a
            // client that bound `wl_output`, which a bar does and this does
            // not.
            ext_workspace_manager_v1::Event::WorkspaceGroup { workspace_group } => {
                let id = workspace_group.id().protocol_id();
                let mut seen = state.seen.lock().unwrap();
                if !seen.groups.contains(&id) {
                    seen.groups.push(id);
                }
            }
            ext_workspace_manager_v1::Event::Done => state.seen.lock().unwrap().done = true,
            ext_workspace_manager_v1::Event::Finished => state.seen.lock().unwrap().done = true,
            _ => {}
        }
    }

    wayland_client::event_created_child!(App, ExtWorkspaceManagerV1, [
        ext_workspace_manager_v1::EVT_WORKSPACE_GROUP_OPCODE => (ExtWorkspaceGroupHandleV1, ()),
        ext_workspace_manager_v1::EVT_WORKSPACE_OPCODE => (ExtWorkspaceHandleV1, ()),
    ]);
}

impl Dispatch<ExtWorkspaceGroupHandleV1, ()> for App {
    fn event(
        state: &mut Self,
        group: &ExtWorkspaceGroupHandleV1,
        event: ext_workspace_group_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let _ = (state, group, event);
    }
}

impl Dispatch<ExtWorkspaceHandleV1, ()> for App {
    fn event(
        state: &mut Self,
        handle: &ExtWorkspaceHandleV1,
        event: ext_workspace_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let object = handle.id().protocol_id();
        let mut seen = state.seen.lock().unwrap();
        if !seen
            .workspaces
            .iter()
            .any(|w| w.handle.id().protocol_id() == object)
        {
            seen.workspaces.push(Known {
                handle: handle.clone(),
                id: String::new(),
                name: String::new(),
                state: String::new(),
            });
        }
        let entry = seen
            .workspaces
            .iter_mut()
            .find(|w| w.handle.id().protocol_id() == object)
            .unwrap();
        match event {
            ext_workspace_handle_v1::Event::Id { id } => entry.id = id,
            ext_workspace_handle_v1::Event::Name { name } => entry.name = name,
            ext_workspace_handle_v1::Event::State { state } => {
                entry.state = format!("{state:?}");
            }
            _ => {}
        }
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut activate = None;
    while let Some(arg) = args.next() {
        if arg == "--activate" {
            activate = args.next();
        }
    }

    let conn = Connection::connect_to_env().expect("no compositor");
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let display = conn.display();
    display.get_registry(&qh, ());

    let mut app = App {
        manager: None,
        seen: Arc::new(Mutex::new(Seen::default())),
        activate,
    };

    queue.roundtrip(&mut app).expect("registry");
    let Some(manager) = app.manager.clone() else {
        eprintln!("this compositor has no ext_workspace_manager_v1");
        std::process::exit(1);
    };

    // Two round trips: one for the objects, one for what they say about
    // themselves. `done` arrives at the end of both.
    for _ in 0..4 {
        queue.roundtrip(&mut app).expect("roundtrip");
        if app.seen.lock().unwrap().done {
            break;
        }
    }

    let seen = app.seen.lock().unwrap().workspaces.clone();
    let groups = app.seen.lock().unwrap().groups.len();
    println!("{} group(s), {} workspace(s)", groups, seen.len());
    for known in &seen {
        println!("  {}  {}  {}", known.id, known.name, known.state);
    }

    if let Some(wanted) = app.activate.clone() {
        let Some(known) = seen.iter().find(|k| k.id == wanted) else {
            eprintln!("no workspace with id {wanted}");
            std::process::exit(1);
        };
        // Ask, then commit: nothing happens until the commit, which is the
        // protocol's way of letting a bar mean several things at once.
        known.handle.activate();
        manager.commit();
        queue.roundtrip(&mut app).expect("commit");
        println!("asked to activate {wanted}; the shell decides what that means");
    }
}
