// SPDX-License-Identifier: GPL-3.0-or-later
//
// Layer-surface policy reload against a real compositor and client.

mod common;

use common::Compositor;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;
use wayland_client::protocol::{wl_compositor, wl_registry, wl_surface};
use wayland_client::{delegate_noop, Connection, Dispatch, QueueHandle};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

#[derive(Default)]
struct LayerClient {
    compositor: Option<wl_compositor::WlCompositor>,
    layer_shell: Option<zwlr_layer_shell_v1::ZwlrLayerShellV1>,
    closed: bool,
}

impl Dispatch<wl_registry::WlRegistry, ()> for LayerClient {
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
            "zwlr_layer_shell_v1" => {
                state.layer_shell = Some(registry.bind(name, version.min(4), qh, ()))
            }
            _ => {}
        }
    }
}

delegate_noop!(LayerClient: ignore wl_compositor::WlCompositor);
delegate_noop!(LayerClient: ignore wl_surface::WlSurface);
delegate_noop!(LayerClient: ignore zwlr_layer_shell_v1::ZwlrLayerShellV1);

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for LayerClient {
    fn event(
        state: &mut Self,
        surface: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure { serial, .. } => surface.ack_configure(serial),
            zwlr_layer_surface_v1::Event::Closed => state.closed = true,
            _ => {}
        }
    }
}

#[test]
fn a_mapped_layer_adopts_reloaded_policy_without_remapping() {
    let dir = PathBuf::from(format!("/tmp/viewport-layer-rules-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("config directory");
    let config = dir.join("config.json");
    std::fs::write(
        &config,
        r#"{
            "layer_rules": [{
                "match": {"namespace": {"contains": "menu"}},
                "opacity": 0.5,
                "capture": false,
                "blur": true,
                "z_index": 3
            }]
        }"#,
    )
    .expect("initial config");

    let compositor = Compositor::builder("reload")
        .prefix("viewport-layer-rules")
        .args(["--watch-config", "--config"])
        .arg(&config)
        .env("VIEWPORT_LOG", "viewport=debug")
        .owning(dir)
        .awaiting("for configuration changes", Duration::from_secs(10))
        .start();
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
    let mut client = LayerClient::default();
    queue.roundtrip(&mut client).expect("Wayland globals");
    let surface = client
        .compositor
        .as_ref()
        .expect("wl_compositor")
        .create_surface(&qh, ());
    let layer = client
        .layer_shell
        .as_ref()
        .expect("zwlr_layer_shell_v1")
        .get_layer_surface(
            &surface,
            None,
            zwlr_layer_shell_v1::Layer::Top,
            "menu".to_owned(),
            &qh,
            (),
        );
    layer.set_size(320, 32);
    surface.commit();
    queue
        .roundtrip(&mut client)
        .expect("initial layer configure");

    let initial = "layer surface \"menu\": policy opacity=0.5 capture=false blur=true z_index=3";
    assert!(
        compositor.saw(initial, Duration::from_secs(10)),
        "the initial rule did not reach the mapped layer:\n{}",
        compositor.log()
    );

    let temporary = config.with_extension("tmp");
    std::fs::write(
        &temporary,
        r#"{
            "layer_rules": [{
                "match": {"namespace": {"equals": "menu"}},
                "opacity": 0.75,
                "capture": true,
                "blur": false,
                "z_index": -4
            }]
        }"#,
    )
    .expect("replacement config");
    std::fs::rename(&temporary, &config).expect("atomic config save");

    let reloaded = "layer surface \"menu\": policy opacity=0.75 capture=true blur=false z_index=-4";
    assert!(
        compositor.saw(reloaded, Duration::from_secs(10)),
        "the mapped layer kept stale policy after reload:\n{}",
        compositor.log()
    );

    let oversized = serde_json::json!({
        "layer_rules": vec![serde_json::json!({
            "match": {"namespace": "menu"},
            "opacity": 0.25
        }); 257]
    });
    std::fs::write(
        &temporary,
        serde_json::to_vec(&oversized).expect("oversized config JSON"),
    )
    .expect("oversized replacement config");
    std::fs::rename(&temporary, &config).expect("atomic oversized config save");
    assert!(
        compositor.saw("maximum is 256", Duration::from_secs(10)),
        "oversized rules were not rejected:\n{}",
        compositor.log()
    );
    assert!(
        !compositor.log().contains("policy opacity=0.25"),
        "a rejected reload replaced live policy:\n{}",
        compositor.log()
    );

    std::fs::write(&temporary, r#"{"layer_rules": []}"#).expect("empty replacement config");
    std::fs::rename(&temporary, &config).expect("atomic empty config save");
    let cleared = "layer surface \"menu\": policy opacity=1 capture=true blur=false z_index=0";
    assert!(
        compositor.saw(cleared, Duration::from_secs(10)),
        "an empty rule array did not clear live policy:\n{}",
        compositor.log()
    );

    queue
        .roundtrip(&mut client)
        .expect("client remains connected");
    assert!(!client.closed, "config reload closed the layer surface");
    assert_eq!(
        compositor
            .log()
            .matches("layer surface \"menu\" on ")
            .count(),
        1,
        "the surface was destructively remapped:\n{}",
        compositor.log()
    );
}
