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
    let (mut backend, winit) =
        winit::init::<GlesRenderer>().map_err(|e| anyhow::anyhow!("winit backend: {e}"))?;

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
        let formats: Vec<_> = backend
            .renderer()
            .dmabuf_formats()
            .iter()
            .copied()
            .collect();
        // Asked for once. Both of the things below want the node behind this
        // display, and enumerating it twice asks the driver the same question
        // twice to get the same answer — or, if the second one ever disagreed
        // with the first, to advertise one GPU to clients and record on
        // another.
        let node = smithay::backend::egl::EGLDevice::device_for_display(
            backend.renderer().egl_context().display(),
        )
        .ok()
        .and_then(|device| device.try_get_render_node().ok().flatten());
        state.advertise_dmabuf(node.map(|node| node.dev_id()), formats.clone());

        // And the same GPU for capture, so a recorder works nested too —
        // which is the only place this can be tested without a second
        // machine.
        state.capture_gpu = node.map(|node| (node, formats));
    }

    // The ping the shell uses to wake the loop, as in `udev.rs`. It has to be
    // in place *before* `start_shell`, because that is where it gets handed
    // over: `start_shell` ends with `if let Some(ping) = self.shell_ping`, so a
    // ping installed afterwards is never given to the shell at all.
    //
    // Missing here until now, and the effect was quiet rather than loud. The
    // shell started, connected, painted, and posted — and nothing ever called
    // `drain_shell`, so every message it sent sat in the mailbox for the life
    // of the session. Windows still appeared, because `state.rs` gives up on
    // the shell after 2500ms and falls back to a built-in layout, which is why
    // this looked like a working desktop rather than a broken one. Anyone
    // developing the shell on this backend was watching the fallback.
    #[cfg(feature = "wpe")]
    {
        let (ping, source) = smithay::reexports::calloop::ping::make_ping()
            .map_err(|e| anyhow::anyhow!("creating the shell ping: {e}"))?;
        event_loop
            .handle()
            .insert_source(source, |_, _, state| state.drain_shell())
            .map_err(|e| anyhow::anyhow!("inserting the shell ping: {e}"))?;
        state.shell_ping = Some(ping);
    }

    // The out-of-process shell, which needs no DRM node from us — it is a
    // client of this compositor like any other, and nesting changes nothing
    // about that.
    state.start_shell_process();

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
                if state.shell_backend == crate::shell_backend::ShellBackend::Wpe {
                    if let Err(e) = state.start_shell(&card, &render) {
                        tracing::warn!("the shell did not start, so this is windows only: {e:#}");
                    }
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
    // Whether the last frame failed to bind. Only so the message is not
    // repeated: a lost GL context does not come back on its own, and a redraw
    // is asked for every frame, so saying it once a frame fills the log at the
    // refresh rate and buries whatever came before it.
    let mut unbound = false;

    event_loop
        .handle()
        .insert_source(winit, move |event, _, state| match event {
            WinitEvent::Resized { size, .. } => {
                output.change_current_state(Some(Mode { size, refresh: 60_000 }), None, None, None);
                state.space.map_output(&output, (0, 0));
                // The shell lays out against the output layout, so a resize it
                // is not told about would leave every window where it was.
                state.notify_output_layout();
                state.advertise_outputs();
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
                // Not an unwrap. This is the development backend, and the thing
                // that takes the context away — a driver reset, a mesa crash,
                // the host compositor going out from under the window — is the
                // thing being debugged when it happens. Ending the session on
                // it destroys the state that would have said why.
                //
                // A frame that cannot bind draws nothing, and then submits
                // nothing and sends no frame callbacks: a client told its frame
                // was shown draws the next one into a compositor that cannot
                // present it. The redraw at the end is still asked for, so a
                // failure that turns out to be transient recovers by itself.
                //
                // The whole frame lives in the `Ok` arm because the bind holds
                // `backend` mutably until the framebuffer it returns is
                // dropped, and both `submit` and `window` want it back.
                let drawn = match backend.bind() {
                    Ok((renderer, mut framebuffer)) => {
                        if unbound {
                            tracing::info!("the nested backend is drawing again");
                            unbound = false;
                        }
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

                        // Screenshots, while the renderer is in hand. After the
                        // draw so a client that asked during this frame is served
                        // with what the frame shows rather than the one before it.
                        state.service_screencopy::<_, smithay::backend::renderer::gles::GlesRenderbuffer>(
                            &output, renderer,
                        );
                        state.service_image_capture::<_, smithay::backend::renderer::gles::GlesRenderbuffer>(
                            &output, renderer,
                        );
                        // No allocator here, so a resized source is renegotiated
                        // onto shared memory — which is what a nested session was
                        // using in any case.
                        state.resize_casts(None::<&mut viewport_vulkan::VulkanRenderer>);
                        state.feed_casts::<_, smithay::backend::renderer::gles::GlesRenderbuffer>(
                            &output, renderer,
                        );
                        true
                    }
                    Err(e) => {
                        // Once per run of them. A lost context does not come
                        // back on its own and a redraw is asked for every
                        // frame, so one message per frame fills the log at the
                        // refresh rate and buries what caused it.
                        if !unbound {
                            tracing::error!("could not bind the nested backend: {e}");
                            unbound = true;
                        }
                        false
                    }
                };
                if !drawn {
                    backend.window().request_redraw();
                    return;
                }
                if let Err(e) = backend.submit(Some(&[damage])) {
                    tracing::error!("submit failed: {e}");
                }

                // WebKit would not paint frame N+1 until frame N was
                // acknowledged; the same discipline applies to clients, and
                // this is where their frame callbacks fire.
                let at = state.start_time.elapsed();
                // Once, not once per window: this walks every surface on the
                // output already.
                state.release_frame_barriers(&output, at);
                state.arm_barrier_tick();
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
                // The lock screen, which is neither of the above and would
                // otherwise draw once and stop.
                for lock in state.lock_surfaces.values() {
                    smithay::desktop::utils::send_frames_surface_tree(
                        lock.wl_surface(),
                        &output,
                        at,
                        Some(Duration::ZERO),
                        |_, _| Some(output.clone()),
                    );
                }

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
