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
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::Color32F;
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

    // The shell, nested. It needs DRM nodes to allocate on — the same GPU the
    // host compositor is using, which is what the EGL device names.
    #[cfg(feature = "wpe")]
    {
        let node = smithay::backend::egl::EGLDevice::device_for_display(
            backend.renderer().egl_context().display(),
        )
        .ok()
        .and_then(|device| device.try_get_render_node().ok().flatten());
        match node {
            Some(render) => {
                // The primary node, for WPE: wpe_drm_device_new asserts on a
                // null one, so a render node alone is not enough.
                let card = render
                    .node_with_type(smithay::backend::drm::NodeType::Primary)
                    .and_then(|node| node.ok())
                    .unwrap_or(render);
                if let Err(e) = state.start_shell(&card, &render) {
                    tracing::warn!("the shell did not start, so this is windows only: {e:#}");
                }
            }
            None => tracing::warn!("no render node for the shell; this is windows only"),
        }
    }

    // Prime the loop. Each redraw asks for the next, so one is enough — but
    // without it the first has to come from winit itself, and when it does not
    // the shell paints into a mailbox nobody drains and the window stays
    // empty.
    backend.window().request_redraw();

    let mut damage_tracker = OutputDamageTracker::from_output(&output);
    let mut dumped: Option<std::time::Instant> = None;

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

                // The same desktop the real backend draws: shell, layer
                // surfaces, windows with their clip, and the pointer. Assembled
                // by the shared path so the two cannot drift — nested showing
                // windows on a flat colour is what that drift looked like.
                #[cfg(feature = "wpe")]
                state.import_shell_frame();
                let frame = state.frame_for(&output);
                {
                    let (renderer, mut framebuffer) = backend.bind().unwrap();
                    let elements = crate::render::build(&frame, renderer);

                    // The same capture the real backend has. Nested is where
                    // this gets used most, and it is the only way to tell "the
                    // shell painted nothing" from "the shell never reached the
                    // screen".
                    if let Some(path) = crate::dump::output_target() {
                        // Repeatedly, overwriting: whatever is on screen when
                        // someone looks at the file is what it holds. A single
                        // capture has to guess when the interesting thing
                        // happens, and it is usually wrong — the first attempt
                        // caught a buffer WebKit had not painted into yet.
                        #[cfg(feature = "wpe")]
                        let painted = state.shell_frames >= 2;
                        #[cfg(not(feature = "wpe"))]
                        let painted = false;
                        let due = dumped
                            .map(|at: std::time::Instant| {
                                at.elapsed() >= std::time::Duration::from_secs(2)
                            })
                            .unwrap_or(true);
                        if due && (painted || !frame.windows.is_empty() || frame.locked_blank) {
                            dumped = Some(std::time::Instant::now());
                            if let Err(e) = crate::dump::output_frame::<
                                _,
                                smithay::backend::renderer::gles::GlesRenderbuffer,
                                _,
                            >(
                                renderer,
                                &elements,
                                size,
                                [0.1, 0.1, 0.1, 1.0],
                                &path,
                            ) {
                                tracing::error!("could not dump the nested output: {e:#}");
                            }
                        }
                    }
                    let result = damage_tracker.render_output(
                        renderer,
                        &mut framebuffer,
                        0,
                        &elements,
                        Color32F::from([0.1, 0.1, 0.1, 1.0]),
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
                let at = state.start_time.elapsed();
                state.space.elements().for_each(|window| {
                    window.send_frame(&output, at, Some(Duration::ZERO), |_, _| {
                        Some(output.clone())
                    })
                });
                smithay::desktop::layer_map_for_output(&output)
                    .layers()
                    .for_each(|layer| {
                        layer.send_frame(&output, at, Some(Duration::ZERO), |_, _| {
                            Some(output.clone())
                        })
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
