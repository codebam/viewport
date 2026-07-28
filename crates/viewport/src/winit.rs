// SPDX-License-Identifier: GPL-3.0-or-later
//
// The nested winit backend. Ports the nested half of src/output.c.
//
// Adapted from smithay's MIT-licensed `smallvil` example.
//
// This is the development backend: it runs Viewport in a window on whatever
// compositor is already running, so windows can be placed and the IPC exercised
// without touching DRM/KMS. The udev backend comes later.

use std::time::Duration;

use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::winit::{self, WinitEvent};
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::EventLoop;
use smithay::utils::{Rectangle, Transform};

use crate::state::ViewportState;

pub fn init(
    event_loop: &mut EventLoop<'static, ViewportState>,
    state: &mut ViewportState,
) -> anyhow::Result<()> {
    let (mut backend, winit) = winit::init::<GlesRenderer>()
        .map_err(|e| anyhow::anyhow!("winit backend: {e}"))?;

    let mode = Mode {
        size: backend.window_size(),
        refresh: 60_000,
    };

    let output = Output::new(
        "winit".to_owned(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "Viewport".into(),
            model: "Winit".into(),
            serial_number: "Unknown".into(),
        },
    );
    let _global = output.create_global::<ViewportState>(&state.display_handle);
    output.change_current_state(
        Some(mode),
        Some(Transform::Flipped180),
        None,
        Some((0, 0).into()),
    );
    output.set_preferred(mode);
    state.space.map_output(&output, (0, 0));
    state.active_output = Some(output.name());

    // A GPU client cannot present without this, whatever the backend. Nested
    // is where most development happens, so it is worth as much here as on
    // real hardware.
    {
        use smithay::backend::renderer::ImportDma as _;
        let formats: Vec<_> = backend.renderer().dmabuf_formats().iter().copied().collect();
        let node = smithay::backend::egl::EGLDevice::device_for_display(
            backend.renderer().egl_context().display(),
        )
        .ok()
        .and_then(|device| device.try_get_render_node().ok().flatten())
        .map(|node| node.dev_id());
        state.advertise_dmabuf(node, formats);
    }

    let mut damage_tracker = OutputDamageTracker::from_output(&output);

    event_loop
        .handle()
        .insert_source(winit, move |event, _, state| match event {
            WinitEvent::Resized { size, .. } => {
                output.change_current_state(Some(Mode { size, refresh: 60_000 }), None, None, None);
                state.space.map_output(&output, (0, 0));
                // The shell lays out against the output layout, so a resize it
                // is not told about would leave every window where it was.
                state.notify_output_layout();
            }

            WinitEvent::Input(event) => state.process_input_event(event),

            WinitEvent::Redraw => {
                let size = backend.window_size();
                let damage = Rectangle::from_size(size);

                {
                    let (renderer, mut framebuffer) = backend.bind().unwrap();
                    let result = smithay::desktop::space::render_output::<
                        _,
                        WaylandSurfaceRenderElement<GlesRenderer>,
                        _,
                        _,
                    >(
                        &output,
                        renderer,
                        &mut framebuffer,
                        1.0,
                        0,
                        [&state.space],
                        &[],
                        &mut damage_tracker,
                        // The shell will own this area once the web engine is
                        // wired up; until then it is just a backdrop.
                        [0.1, 0.1, 0.1, 1.0],
                    );
                    if let Err(e) = result {
                        tracing::error!("render failed: {e}");
                    }
                }
                if let Err(e) = backend.submit(Some(&[damage])) {
                    tracing::error!("submit failed: {e}");
                }

                // WebKit would not paint frame N+1 until frame N was
                // acknowledged; the same discipline applies to clients, and
                // this is where their frame callbacks fire.
                state.space.elements().for_each(|window| {
                    window.send_frame(
                        &output,
                        state.start_time.elapsed(),
                        Some(Duration::ZERO),
                        |_, _| Some(output.clone()),
                    )
                });

                state.space.refresh();
                state.popups.cleanup();
                let _ = state.display_handle.flush_clients();

                backend.window().request_redraw();
            }

            WinitEvent::CloseRequested => state.loop_signal.stop(),

            _ => {}
        })
        .map_err(|e| anyhow::anyhow!("insert winit source: {e}"))?;

    Ok(())
}
