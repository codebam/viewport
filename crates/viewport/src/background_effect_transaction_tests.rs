// SPDX-License-Identifier: GPL-3.0-or-later

use super::{
    committed_region, ensure_commit_hook_for, reject_oversized_pending_region, resolve_region,
    SurfaceEffectData, MAX_REGION_OPERATIONS,
};
use std::os::unix::net::UnixStream;
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use smithay::reexports::wayland_server::{
    backend::ClientData, protocol::wl_surface::WlSurface as ServerSurface, Client as ServerClient,
    Display, DisplayHandle,
};
use smithay::utils::{Logical, Rectangle};
use smithay::wayland::background_effect::BackgroundEffectSurfaceCachedState;
use smithay::wayland::compositor::{
    add_blocker, with_states, Barrier, CompositorClientState, CompositorHandler, CompositorState,
    RectangleKind, RegionAttributes,
};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{
    wl_compositor::WlCompositor as ClientCompositor, wl_registry,
    wl_surface::WlSurface as ClientSurface,
};
use wayland_client::{Connection, Dispatch, QueueHandle};

type CommittedRects = Option<Vec<Rectangle<i32, Logical>>>;

fn current_effect_rects(surface: &ServerSurface) -> CommittedRects {
    with_states(surface, |states| {
        let data = states.data_map.get::<SurfaceEffectData>()?;
        let data = data.0.lock().unwrap_or_else(|error| error.into_inner());
        data.rects.as_ref().map(|rects| rects.as_ref().clone())
    })
}

fn current_cached_rects(surface: &ServerSurface) -> CommittedRects {
    with_states(surface, |states| {
        committed_region(states).map(|region| resolve_region(&region))
    })
}

struct TestCompositorState {
    compositor_state: CompositorState,
    surface: Option<ServerSurface>,
    post_commit_rects: Vec<CommittedRects>,
    handler_commits: usize,
}

impl CompositorHandler for TestCompositorState {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a ServerClient) -> &'a CompositorClientState {
        &client
            .get_data::<ServerClientData>()
            .expect("test client data")
            .compositor_state
    }

    fn new_surface(&mut self, surface: &ServerSurface) {
        self.surface = Some(surface.clone());
    }

    fn commit(&mut self, _surface: &ServerSurface) {
        self.handler_commits += 1;
    }
}

impl AsMut<CompositorState> for TestCompositorState {
    fn as_mut(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }
}

smithay::delegate_dispatch2!(TestCompositorState);

#[derive(Default)]
struct ServerClientData {
    compositor_state: CompositorClientState,
}

impl ClientData for ServerClientData {}

struct ClientSideState;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for ClientSideState {
    fn event(
        _state: &mut Self,
        _registry: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _connection: &Connection,
        _queue_handle: &QueueHandle<Self>,
    ) {
    }
}

wayland_client::delegate_noop!(ClientSideState: ClientCompositor);
wayland_client::delegate_noop!(ClientSideState: ignore ClientSurface);

type ServerCommand =
    Box<dyn FnOnce(&mut TestCompositorState, &DisplayHandle, &ServerClientData) + Send + 'static>;

fn spawn_server(socket: UnixStream) -> (mpsc::Sender<ServerCommand>, JoinHandle<()>) {
    let (command_tx, command_rx) = mpsc::channel::<ServerCommand>();
    let (ready_tx, ready_rx) = mpsc::sync_channel(0);

    let thread = thread::spawn(move || {
        let mut display = Display::<TestCompositorState>::new().expect("test display");
        let mut display_handle = display.handle();
        let compositor_state = CompositorState::new::<TestCompositorState>(&display_handle);
        let mut state = TestCompositorState {
            compositor_state,
            surface: None,
            post_commit_rects: Vec::new(),
            handler_commits: 0,
        };

        let client_data = Arc::new(ServerClientData::default());
        let display_client_data: Arc<dyn ClientData> = client_data.clone();
        let _client = display_handle
            .insert_client(socket, display_client_data)
            .expect("insert test client");

        ready_tx.send(()).expect("announce test server");
        loop {
            match command_rx.recv_timeout(Duration::from_millis(1)) {
                Ok(command) => command(&mut state, &display_handle, client_data.as_ref()),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            display
                .dispatch_clients(&mut state)
                .expect("dispatch test clients");
            display.flush_clients().expect("flush test clients");
        }
    });

    ready_rx.recv().expect("test server ready");
    (command_tx, thread)
}

fn server_call<R>(
    commands: &mpsc::Sender<ServerCommand>,
    operation: impl FnOnce(&mut TestCompositorState, &DisplayHandle, &ServerClientData) -> R
        + Send
        + 'static,
) -> R
where
    R: Send + 'static,
{
    let (reply_tx, reply_rx) = mpsc::sync_channel(0);
    commands
        .send(Box::new(move |state, display_handle, client_data| {
            reply_tx
                .send(operation(state, display_handle, client_data))
                .unwrap_or_else(|_| panic!("test caller dropped reply channel"));
        }))
        .expect("test Wayland server stopped");
    reply_rx.recv().expect("test server reply")
}

fn stage_background_effect(
    commands: &mpsc::Sender<ServerCommand>,
    region: RegionAttributes,
    blocker: Barrier,
) {
    server_call(commands, move |state, _, _| {
        let surface = state
            .surface
            .as_ref()
            .expect("client surface was created")
            .clone();
        ensure_commit_hook_for::<TestCompositorState, _>(&surface, |state, surface| {
            state.post_commit_rects.push(current_effect_rects(surface));
        });
        with_states(&surface, move |states| {
            let mut cached = states
                .cached_state
                .get::<BackgroundEffectSurfaceCachedState>();
            cached.pending().blur_region = Some(region);
        });
        add_blocker(&surface, blocker);
    });
}

fn stage_unblocked_background_effect(
    commands: &mpsc::Sender<ServerCommand>,
    region: RegionAttributes,
    reject_oversized: bool,
) {
    server_call(commands, move |state, _, _| {
        let surface = state
            .surface
            .as_ref()
            .expect("client surface was created")
            .clone();
        ensure_commit_hook_for::<TestCompositorState, _>(&surface, |state, surface| {
            state.post_commit_rects.push(current_effect_rects(surface));
        });
        with_states(&surface, |states| {
            states
                .cached_state
                .get::<BackgroundEffectSurfaceCachedState>()
                .pending()
                .blur_region = Some(region.clone());
        });
        assert_eq!(
            reject_oversized_pending_region(&surface, &region),
            reject_oversized
        );
    });
}

fn drain_transactions(commands: &mpsc::Sender<ServerCommand>) {
    server_call(commands, |state, display_handle, client_data| {
        client_data
            .compositor_state
            .blocker_cleared(state, display_handle);
    });
}

#[derive(Debug, PartialEq)]
struct CommitSnapshot {
    post_commit_rects: Vec<CommittedRects>,
    handler_commits: usize,
    effect_rects: CommittedRects,
    cached_rects: CommittedRects,
}

fn snapshot(commands: &mpsc::Sender<ServerCommand>) -> CommitSnapshot {
    server_call(commands, |state, _, _| {
        let surface = state.surface.as_ref().expect("client surface was created");
        CommitSnapshot {
            post_commit_rects: state.post_commit_rects.clone(),
            handler_commits: state.handler_commits,
            effect_rects: current_effect_rects(surface),
            cached_rects: current_cached_rects(surface),
        }
    })
}

fn rect(x: i32, y: i32, width: i32, height: i32) -> Rectangle<i32, Logical> {
    Rectangle::new((x, y).into(), (width, height).into())
}

#[test]
fn blocked_background_effect_commits_apply_in_order() {
    let (client_socket, server_socket) = UnixStream::pair().expect("socket pair");
    let (commands, server_thread) = spawn_server(server_socket);
    let connection = Connection::from_socket(client_socket).expect("test client connection");
    let (globals, mut event_queue) =
        registry_queue_init::<ClientSideState>(&connection).expect("test registry");
    let queue_handle = event_queue.handle();
    let compositor: ClientCompositor = globals
        .bind(&queue_handle, 1..=5, ())
        .expect("wl_compositor");
    let client_surface: ClientSurface = compositor.create_surface(&queue_handle, ());
    let mut client_state = ClientSideState;
    event_queue
        .roundtrip(&mut client_state)
        .expect("create test surface");

    let region_a = RegionAttributes {
        rects: vec![(RectangleKind::Add, rect(1, 2, 30, 40))],
    };
    let region_b = RegionAttributes {
        rects: vec![(RectangleKind::Add, rect(10, 20, 50, 60))],
    };
    let expected_a = resolve_region(&region_a);
    let expected_b = resolve_region(&region_b);
    let blocker_a = Barrier::new(false);
    let blocker_b = Barrier::new(false);

    stage_background_effect(&commands, region_a, blocker_a.clone());
    client_surface.commit();
    event_queue
        .roundtrip(&mut client_state)
        .expect("queue commit A");

    stage_background_effect(&commands, region_b, blocker_b.clone());
    client_surface.commit();
    event_queue
        .roundtrip(&mut client_state)
        .expect("queue commit B");

    let queued = snapshot(&commands);
    assert!(queued.post_commit_rects.is_empty());
    assert_eq!(queued.handler_commits, 0);
    assert_eq!(queued.effect_rects, None);
    assert_eq!(queued.cached_rects, None);

    blocker_b.signal();
    drain_transactions(&commands);
    let b_released = snapshot(&commands);
    assert!(b_released.post_commit_rects.is_empty());
    assert_eq!(b_released.handler_commits, 0);
    assert_eq!(b_released.effect_rects, None);
    assert_eq!(b_released.cached_rects, None);

    blocker_a.signal();
    drain_transactions(&commands);
    let applied = snapshot(&commands);
    assert_eq!(
        applied.post_commit_rects,
        vec![Some(expected_a), Some(expected_b.clone())]
    );
    assert_eq!(applied.handler_commits, 2);
    assert_eq!(applied.effect_rects, Some(expected_b.clone()));
    assert_eq!(applied.cached_rects, Some(expected_b));

    drop(commands);
    server_thread.join().expect("test server thread");
}

#[test]
fn oversized_region_clears_pending_state_at_the_commit_boundary() {
    let (client_socket, server_socket) = UnixStream::pair().expect("socket pair");
    let (commands, server_thread) = spawn_server(server_socket);
    let connection = Connection::from_socket(client_socket).expect("test client connection");
    let (globals, mut event_queue) =
        registry_queue_init::<ClientSideState>(&connection).expect("test registry");
    let queue_handle = event_queue.handle();
    let compositor: ClientCompositor = globals
        .bind(&queue_handle, 1..=5, ())
        .expect("wl_compositor");
    let client_surface: ClientSurface = compositor.create_surface(&queue_handle, ());
    let mut client_state = ClientSideState;
    event_queue
        .roundtrip(&mut client_state)
        .expect("create test surface");

    let first = RegionAttributes {
        rects: vec![(RectangleKind::Add, rect(1, 2, 30, 40))],
    };
    let final_region = RegionAttributes {
        rects: vec![(RectangleKind::Add, rect(10, 20, 50, 60))],
    };
    let oversized = RegionAttributes {
        rects: vec![(RectangleKind::Add, rect(0, 0, 1, 1)); MAX_REGION_OPERATIONS + 1],
    };
    let expected_first = resolve_region(&first);
    let expected_final = resolve_region(&final_region);

    stage_unblocked_background_effect(&commands, first, false);
    client_surface.commit();
    event_queue
        .roundtrip(&mut client_state)
        .expect("commit first region");

    stage_unblocked_background_effect(&commands, oversized, true);
    client_surface.commit();
    event_queue
        .roundtrip(&mut client_state)
        .expect("commit rejected region");
    let rejected = snapshot(&commands);
    assert_eq!(rejected.post_commit_rects, vec![Some(expected_first), None]);
    assert_eq!(rejected.effect_rects, None);
    assert_eq!(rejected.cached_rects, None);

    stage_unblocked_background_effect(&commands, final_region, false);
    client_surface.commit();
    event_queue
        .roundtrip(&mut client_state)
        .expect("commit final region");
    let recovered = snapshot(&commands);
    assert_eq!(recovered.handler_commits, 3);
    assert_eq!(recovered.effect_rects, Some(expected_final.clone()));
    assert_eq!(recovered.cached_rects, Some(expected_final));

    drop(commands);
    server_thread.join().expect("test server thread");
}
