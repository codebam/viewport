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

    // The host's shortcuts, held back while this window has the keyboard.
    //
    // Without it a compositor inside a compositor never sees Mod4+anything:
    // the host takes the chord first and the nested session is left testing
    // whichever bindings its host happens not to use. See `crate::capture`.
    //
    // Best-effort on purpose. A host that does not implement the protocol is
    // not a failure to start a session with, it is one line in the log and a
    // nested run that behaves as it always did.
    state.capture = {
        use raw_window_handle::{
            HasDisplayHandle as _, HasWindowHandle as _, RawDisplayHandle, RawWindowHandle,
        };
        let window = backend.window();
        let display = window.display_handle().ok().map(|h| h.as_raw());
        let surface = window.window_handle().ok().map(|h| h.as_raw());
        match (display, surface) {
            (Some(RawDisplayHandle::Wayland(d)), Some(RawWindowHandle::Wayland(w))) => {
                // SAFETY: both pointers come from the window this backend owns
                // and outlive the capture, which is dropped with the session.
                match unsafe {
                    crate::capture::keep_the_keys(d.display.as_ptr(), w.surface.as_ptr())
                } {
                    Ok(capture) => Some(capture),
                    Err(e) => {
                        tracing::info!("the host keeps its own shortcuts: {e}");
                        None
                    }
                }
            }
            _ => None,
        }
    };

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
    // And the wallpaper terminal, if one was asked for. Beside the shell
    // because it needs the same thing the shell does: outputs, so it can be
    // told how big the desktop is.
    state.start_background_process();

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
                output.set_preferred(Mode { size, refresh: 60_000 });
                state.space.map_output(&output, (0, 0));
                // The same reshaping the DRM backend does when a mode changes,
                // and it is not optional here either.
                //
                // The layer map caches a usable area, worked out against the
                // output it was last arranged for. Nothing re-arranged it on a
                // resize, so it kept the area of the window winit opened —
                // 1280x800, its default — for the life of the session. The
                // *output* was the right size all along and `usable_width` was
                // not, and the shell lays its windows out inside the usable
                // area: a nested window came out with the desktop drawn to the
                // proportions of some other rectangle entirely, which read as
                // the nested backend ignoring its window and using the
                // monitor's shape.
                state.output_reshaped(&output);
                // The shell lays out against the output layout, so a resize it
                // is not told about would leave every window where it was.
                state.notify_output_layout();
                state.advertise_outputs();
            }

            WinitEvent::Input(event) => state.process_input_event(event),

            WinitEvent::Redraw => {
                let size = backend.window_size();
                let damage = Rectangle::from_size(size);
                // What the frame ended up containing, filled in by the render
                // below and read by the presentation feedback after the submit.
                let mut drawn_states = None;

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
                            // A wallpaper terminal counts as something worth
                            // capturing: on the out-of-process backends there
                            // is no `shell_frames` to wait for, and a desktop
                            // with nothing but a wallpaper on it is exactly
                            // the case someone dumps a frame to look at.
                            if due
                                && (painted
                                    || !frame.windows.is_empty()
                                    || frame.background.is_some()
                                    || frame.locked_blank)
                            {
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
                        match result {
                            // Kept for the presentation feedback below, which
                            // needs to know which surfaces were in the frame.
                            Ok(result) => drawn_states = Some(result.states),
                            Err(e) => tracing::error!("render failed: {e}"),
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

                // The frame is on the host's screen, near enough. Say so.
                //
                // The DRM backend has answered `wp_presentation` since
                // `presentation_feedback` in `udev.rs`; this one never did, and
                // for a client that paces itself on presentation rather than on
                // frame callbacks that is a freeze rather than a slowdown.
                //
                // The shell is exactly such a client on this backend: GTK4
                // presents through Mesa's Vulkan WSI, which will not acquire the
                // next swapchain image until the last one is reported presented.
                // Measured with WAYLAND_DEBUG — one commit, one frame callback
                // answered, zero `presented`, and no second frame for the life
                // of the session. A page with a running animation looked
                // identical to a page that had finished loading.
                if let Some(states) = drawn_states.as_ref() {
                    use smithay::desktop::utils::{
                        surface_presentation_feedback_flags_from_states,
                        take_presentation_feedback_surface_tree, OutputPresentationFeedback,
                    };
                    let mut feedback = OutputPresentationFeedback::new(&output);
                    // The output outright rather than `surface_primary_scanout_output`,
                    // for both reasons `udev.rs` gives: nothing here writes the
                    // scanout state that helper reads, and there is one output —
                    // if this window presented, everything in it presented.
                    for window in state.space.elements() {
                        window.take_presentation_feedback(
                            &mut feedback,
                            |_, _| Some(output.clone()),
                            |surface, _| {
                                surface_presentation_feedback_flags_from_states(
                                    surface, None, states,
                                )
                            },
                        );
                    }
                    for layer in smithay::desktop::layer_map_for_output(&output).layers() {
                        layer.take_presentation_feedback(
                            &mut feedback,
                            |_, _| Some(output.clone()),
                            |surface, _| {
                                surface_presentation_feedback_flags_from_states(
                                    surface, None, states,
                                )
                            },
                        );
                    }
                    for lock in state.lock_surfaces.values() {
                        take_presentation_feedback_surface_tree(
                            lock.wl_surface(),
                            &mut feedback,
                            |_, _| Some(output.clone()),
                            |surface, _| {
                                surface_presentation_feedback_flags_from_states(
                                    surface, None, states,
                                )
                            },
                        );
                    }
                    for surface in state.shell_client_surfaces() {
                        take_presentation_feedback_surface_tree(
                            &surface,
                            &mut feedback,
                            |_, _| Some(output.clone()),
                            |surface, _| {
                                surface_presentation_feedback_flags_from_states(
                                    surface, None, states,
                                )
                            },
                        );
                    }
                    // A software clock: there is no vblank of our own here, and
                    // the host's is not something this backend is told about.
                    // `Vsync` alone, without `HwClock` — claiming a provenance
                    // this number does not have is what `udev.rs` calls out.
                    let now = smithay::reexports::rustix::time::clock_gettime(
                        smithay::reexports::rustix::time::ClockId::Monotonic,
                    );
                    let clock = Duration::new(now.tv_sec as u64, now.tv_nsec as u32);
                    use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind;
                    feedback.presented::<_, smithay::utils::Monotonic>(
                        clock,
                        smithay::wayland::presentation::Refresh::Fixed(state.frame_interval()),
                        0,
                        Kind::Vsync,
                    );
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
                // The out-of-process shell, which is neither of those either —
                // it is drawn under the desktop from its own buffer rather than
                // mapped into the space, so nothing above reaches it. Headless
                // already invites it by name; without the same thing here the
                // shell paints one frame and stops, which reads as "the page
                // loaded and then froze".
                for surface in state.shell_client_surfaces() {
                    smithay::desktop::utils::send_frames_surface_tree(
                        &surface,
                        &output,
                        at,
                        Some(Duration::ZERO),
                        |_, _| Some(output.clone()),
                    );
                }
                // The wallpaper terminal, which is neither of the above and
                // would otherwise draw once and stop.
                if let Some(surface) = state.background_surface().cloned() {
                    smithay::desktop::utils::send_frames_surface_tree(
                        &surface,
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
