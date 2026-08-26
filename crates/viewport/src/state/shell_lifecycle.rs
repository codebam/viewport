// SPDX-License-Identifier: GPL-3.0-or-later
//
// The shell client identity, lifecycle, frame import and restart paths.
// Included by `state.rs` so these impls remain in the `state` module and can
// share its imports while the subsystem is kept to a reviewable file.

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
    /// Set when the client connected through a socket a sandbox asked for, and
    /// carries what the sandbox said about itself. Nothing is refused on the
    /// strength of it yet — the point of the protocol is that a compositor
    /// *can* tell, and a compositor that cannot tell has no way to start.
    pub security_context: Option<smithay::wayland::security_context::SecurityContext>,
    /// Set on the one connection the compositor made for the shell process it
    /// started itself.
    ///
    /// This is what makes the out-of-process shell unforgeable. Recognising it
    /// by `app_id` would mean any client that named itself `dev.viewport.shell`
    /// could take the desktop's place — draw under every window, receive every
    /// click that misses one — and an `app_id` is a string a client chooses.
    /// A connection is not: this one was handed to a process the compositor
    /// spawned, over a socket pair nothing else has an end of.
    pub shell: bool,
    /// Which of them, when there is more than one.
    ///
    /// `--url` on a multi-monitor session runs two pages — the one asked for
    /// and the desktop — and both are shells by the test above. Their
    /// connections are what tells them apart, for the same reason the flag
    /// above is a connection rather than an `app_id`.
    pub shell_id: Option<u32>,
    /// Set on the one connection made for the wallpaper terminal, and
    /// unforgeable for the same reason `shell` is.
    ///
    /// What it buys is the opposite of what `shell` buys: it takes capability
    /// away rather than granting it. A client carrying this is never made a
    /// view, never enters the `Space` and is never a focus target, so nothing
    /// typed or clicked can reach it. See `crate::background`.
    pub background: bool,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

impl ViewportState {
    /// Stop the compositor.
    ///
    /// calloop's signal ends its own dispatch, which under the web engine is
    /// only the inner loop — so the outer GLib loop has to be told as well or
    /// quitting does nothing visible.
    pub fn shutdown(&mut self) {
        // One loop to stop now. This used to have to stop GLib as well, which
        // owned the outer loop and carried on happily when only calloop was
        // told — the bug behind `--exit-after` reporting its deadline and then
        // running for ever.
        self.loop_signal.stop();
    }
}

#[cfg(feature = "wpe")]
impl ViewportState {
    /// Start the shell on the same GPU the renderer uses.
    ///
    /// The formats offered to WebKit are the renderer's own importable set. A
    /// format the compositor cannot import produces a shell that never
    /// appears rather than an error, so asking the renderer is the only
    /// honest way to build that list.
    pub fn start_shell(
        &mut self,
        card: &smithay::backend::drm::DrmNode,
        render: &smithay::backend::drm::DrmNode,
    ) -> anyhow::Result<()> {
        // A renderer of the compositor's own, on the render node, for copying
        // WebKit's frames into buffers it owns.
        //
        // Not the backend's: the copy is about owning the buffer rather than
        // about the output, and nesting under another compositor has no DRM
        // renderer at all. Both backends then import the copy into whatever
        // they draw with, which is what lets the nested one show the desktop.
        // The copy is not optional: `shell_owned` is what the compositor draws,
        // and without it the shell is absent from every frame. What is
        // best-effort is which renderer performs it — Vulkan on this GPU where
        // there is one, OpenGL on this GPU otherwise. Releasing WebKit's buffer
        // back to it depends on the copy having happened, so there is no
        // "skip the copy and show the buffer" to fall back to: the engine
        // paints into the picture on screen, and the alternative to that is
        // holding the buffer, which deadlocks the engine after one frame.
        if self.shell_renderer.is_none() && !self.shell_copy_refused {
            let make = || -> anyhow::Result<(
                crate::udev::Gpu,
                smithay::backend::allocator::gbm::GbmAllocator<smithay::backend::drm::DrmDeviceFd>,
            )> {
                // VIEWPORT_RENDERER=gles means this renderer too. It steered
                // only the outputs before, which left a session forced onto
                // OpenGL still copying the shell's frames with Vulkan — the one
                // renderer the switch exists to take out of the picture, and
                // the one whose failure to import the copy is the reason to
                // reach for the switch at all.
                let forced_gles = crate::udev::renderer_forced_gles();
                let device = if forced_gles {
                    Err(anyhow::anyhow!("VIEWPORT_RENDERER asked for OpenGL"))
                } else {
                    let instance = smithay::backend::vulkan::Instance::new(
                        smithay::backend::vulkan::version::Version::VERSION_1_3,
                        None,
                    )
                    .map_err(|e| {
                        anyhow::anyhow!("creating a vulkan instance for the shell: {e}")
                    })?;
                    // The device borrows nothing from the instance: Smithay's
                    // `PhysicalDevice` holds its own handle to it, which is what
                    // lets the instance be built inside this branch.
                    viewport_vulkan::Device::for_node_exactly(&instance, render)
                        .map_err(|e| anyhow::anyhow!("opening a vulkan device for the shell: {e}"))
                };
                // With an allocator: the copy needs somewhere of its own to draw
                // into, and a renderer without one cannot make an offscreen at
                // all — which presents as "no image to copy the shell's frame
                // into" on the first frame.
                //
                // The render node opens directly rather than through the session:
                // it needs no DRM master, which is the whole difference between it
                // and the card node.
                let path = render
                    .dev_path()
                    .ok_or_else(|| anyhow::anyhow!("the render node has no device path"))?;
                let file = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&path)
                    .map_err(|e| {
                        anyhow::anyhow!("opening {} for the shell: {e}", path.display())
                    })?;
                // Through `DrmDeviceFd` rather than holding the `File`: it is
                // an `Arc` around the descriptor and so clones, and both the
                // GBM device and the allocator taken from it have to.
                let fd = smithay::backend::drm::DrmDeviceFd::new(smithay::utils::DeviceFd::from(
                    std::os::fd::OwnedFd::from(file),
                ));
                let gbm = smithay::backend::allocator::gbm::GbmDevice::new(fd)
                    .map_err(|e| anyhow::anyhow!("creating a gbm device for the shell: {e}"))?;
                let allocator = smithay::backend::allocator::gbm::GbmAllocator::new(
                    gbm.clone(),
                    smithay::backend::allocator::gbm::GbmBufferFlags::RENDERING,
                );
                // Vulkan on *this* device or OpenGL on this device — never
                // Vulkan on some other one. `for_node_exactly` is the whole
                // difference: the loose `for_node` falls back to any Vulkan
                // device there is, and in a virtual machine that is lavapipe,
                // which owns no DRM node and cannot see these buffers.
                let renderer = match device {
                    Ok(device) => viewport_vulkan::VulkanRenderer::with_allocator(
                        &device,
                        allocator.clone(),
                    )
                    .map(crate::udev::Gpu::Vulkan)
                    .map_err(|e| anyhow::anyhow!("creating a vulkan renderer: {e}"))?,
                    Err(_) if forced_gles => {
                        tracing::info!(
                            "VIEWPORT_RENDERER: copying the shell's frames with OpenGL too"
                        );
                        crate::udev::Gpu::Gles(Box::new(crate::udev::gles_renderer(&gbm)?))
                    }
                    Err(e) => {
                        tracing::info!(
                            "no Vulkan on the shell's GPU ({e:#}); copying its frames with OpenGL"
                        );
                        crate::udev::Gpu::Gles(Box::new(crate::udev::gles_renderer(&gbm)?))
                    }
                };
                Ok((renderer, allocator))
            };
            match make() {
                Ok((renderer, allocator)) => {
                    self.shell_renderer = Some(renderer);
                    self.shell_allocator = Some(allocator);
                }
                Err(e) => {
                    tracing::warn!(
                        "no renderer to copy the shell's frame with ({e:#}); \
                         the shell will not be drawn"
                    );
                    self.shell_copy_refused = true;
                }
            }
        }

        let formats: Vec<(u32, u64)> = self
            .shell_renderer
            .as_ref()
            .map(|renderer| renderer.dmabuf_formats())
            .or_else(|| {
                self.udev
                    .as_ref()
                    .map(|udev| udev.primary().renderer.dmabuf_formats())
            })
            .unwrap_or_default()
            .iter()
            // Colour only. The importable set now includes the YUV formats a
            // video decoder produces, and WebKit picks whatever it is offered:
            // a shell allocated as NV12 would be a desktop painted into a luma
            // plane, which imports without complaint and looks like a
            // greyscale smear.
            .filter(|format| !viewport_vulkan::format::is_yuv(format.code))
            .map(|format| (format.code as u32, u64::from(format.modifier)))
            .collect();
        anyhow::ensure!(!formats.is_empty(), "the renderer imports no dmabuf format");

        let (Some(card_path), Some(render_path)) = (card.dev_path(), render.dev_path()) else {
            anyhow::bail!("the drm nodes have no device paths");
        };

        let console = std::env::var("VIEWPORT_LOG")
            .map(|level| level.contains("debug") || level.contains("trace"))
            .unwrap_or(false);

        let size = self.layout_size();
        anyhow::ensure!(
            size.0 > 0 && size.1 > 0,
            "the shell needs an output to size itself against"
        );

        // Which pages, where, and which of them runs the desktop. The same
        // decision the out-of-process backend makes, from the same function:
        // `--url` on a session with more than one monitor is that page on the
        // first screen and the shipped shell on the rest. See
        // `shell_client::plan_shells`.
        let planned = self.plan_shells();
        let mut started = Vec::with_capacity(planned.len());
        for plan in planned {
            let size = (
                plan.region.size.w.max(0) as u32,
                plan.region.size.h.max(0) as u32,
            );
            if size.0 == 0 || size.1 == 0 {
                tracing::warn!("not starting {}: it was given no room", plan.url);
                continue;
            }
            tracing::info!(
                "starting the shell at {}, {}x{}{}",
                plan.url,
                size.0,
                size.1,
                if plan.desktop { "" } else { " (page only)" }
            );
            let engine = crate::shell::Shell::start(
                &card_path,
                &render_path,
                &formats,
                size,
                &plan.url,
                console,
            )?;
            if let Some(ping) = self.shell_ping.clone() {
                engine.wake_with(ping);
            }
            started.push(crate::shell::Page {
                engine,
                url: plan.url,
                region: plan.region,
                desktop: plan.desktop,
                size: Some(size),
                owned: None,
                damage: Default::default(),
                element_id: smithay::backend::renderer::element::Id::new(),
                restarts: 0,
                restart_window: None,
                announced: false,
            });
        }
        self.shells = started;
        Ok(())
    }
}

#[cfg(feature = "wpe")]
impl ViewportState {
    /// The modifiers the shell's copy buffer may be allocated with.
    ///
    /// The intersection of what the copy renderer can draw into and what the
    /// renderer that draws the desktop can sample from, because the buffer is
    /// handed from one to the other. They are usually the same device and
    /// usually the same renderer, but not always: the copy runs on the render
    /// node's own renderer, and under a nested backend the desktop is drawn by
    /// a renderer that never saw that node.
    ///
    /// Empty when there is nothing in common — or when neither advertises a
    /// modifier at all, which is an OpenGL driver without the modifier
    /// extensions. [`crate::dump::owned_image`] allocates implicitly then,
    /// which is what such a driver wants.
    fn shell_copy_modifiers(&self) -> Vec<smithay::backend::allocator::Modifier> {
        let Some(copy) = self.shell_renderer.as_ref() else {
            return Vec::new();
        };
        let importable = |formats: smithay::backend::allocator::format::FormatSet| {
            formats
                .iter()
                .filter(|format| format.code == smithay::backend::allocator::Fourcc::Argb8888)
                .map(|format| format.modifier)
                .collect::<Vec<_>>()
        };
        let mine = importable(copy.dmabuf_formats());
        let Some(theirs) = self
            .udev
            .as_ref()
            .map(|udev| importable(udev.primary().renderer.dmabuf_formats()))
        else {
            // No DRM renderer to hand it to: nested, where the backend's own
            // renderer imports it and the copy renderer's set is the only one
            // this side of the compositor knows.
            return mine;
        };
        let both: Vec<_> = mine
            .iter()
            .copied()
            .filter(|modifier| theirs.contains(modifier))
            .collect();
        if both.is_empty() && !mine.is_empty() {
            // Two renderers on one GPU with no ARGB8888 modifier in common
            // should not happen, and if it does the buffer cannot both be
            // drawn into and be sampled from whatever it is allocated as. The
            // copy renderer wins, because a copy that fails is a shell that is
            // never drawn at all, while an import that fails says so per
            // output and names the modifier.
            tracing::warn!(
                "the shell's copy renderer and the display's share no ARGB8888 modifier; \
                 allocating for the copy"
            );
            return mine;
        }
        both
    }

    /// Import whatever the shell last painted, as a texture.
    ///
    /// The imported texture is cached: WebKit paints only when something
    /// changed, so most frames reuse the previous one, and re-importing a
    /// buffer that has not changed would mean a vkCreateImage per output per
    /// frame.
    ///
    /// The presented frame is acknowledged here rather than after the commit.
    /// That is a simplification — strictly WebKit should be released once the
    /// pixels are on screen — and it means the engine may run one frame ahead
    /// of the display.
    pub fn import_shell_frame(&mut self) {
        // By index, because the copy below hands the renderer back out of
        // `self` and then reaches into it again: a borrow of one page held
        // across that is a borrow of the whole compositor.
        for at in 0..self.shells.len() {
            self.import_page_frame(at);
        }
    }

    /// The same, for one page.
    fn import_page_frame(&mut self, page: usize) {
        use smithay::backend::allocator::Buffer as _;
        use smithay::backend::renderer::ImportDma as _;

        if let Some(pending) = self.shells[page].engine.take_frame() {
            let size: smithay::utils::Size<i32, smithay::utils::Physical> = (
                pending.buffer.width() as i32,
                pending.buffer.height() as i32,
            )
                .into();
            let first = self.shells[page].owned.is_none();

            // The whole buffer, because WebKit's per-frame damage rectangles
            // are not carried across the shim. Redrawing more than changed
            // costs a composite; reporting none at all stops the output.
            self.shells[page]
                .damage
                .add([smithay::utils::Rectangle::from_size(
                    size.to_logical(1)
                        .to_buffer(1, smithay::utils::Transform::Normal),
                )]);

            // Allocated before the old one is given up, not after.
            //
            // The old buffer is the picture on screen. Taking it first and then
            // failing to replace it — the layout changed and the device is out
            // of memory, or the renderer is gone — drops the shell out of the
            // render list entirely, which is a grey half of a desktop that
            // comes back only if WebKit paints again. Holding a stale frame is
            // the better failure: it is wrong by one layout, not absent.
            let stale = match self.shells[page].owned.as_ref() {
                Some((_, at)) => *at != size,
                // First frame.
                None => true,
            };
            if stale {
                // Two renderers touch this buffer: the shell's copies into it,
                // and the output's samples from it. Only a modifier both of
                // them advertise works, and on a machine where one is Vulkan
                // that rules out the implicit one entirely — see `owned_image`.
                let modifiers = self.shell_copy_modifiers();
                match self
                    .shell_allocator
                    .as_mut()
                    .map(|allocator| crate::dump::owned_image(allocator, size, &modifiers))
                {
                    Some(Ok(buffer)) => self.shells[page].owned = Some((buffer, size)),
                    Some(Err(e)) => tracing::error!(
                        "could not allocate a {}x{} image for the shell's frame: {e:#}",
                        size.w,
                        size.h
                    ),
                    None => tracing::error!("no allocator for the shell's frame"),
                }
            }

            // Import and copy in one place, because the texture belongs to the
            // renderer that made it: a Vulkan texture and a GLES texture share
            // a trait and nothing else, so the copy has to happen while that
            // renderer is still in hand. Taken out of `self` for the duration
            // so the body can reach the rest of it.
            let mut renderer = self.shell_renderer.take();
            if let Some(gpu) = renderer.as_mut() {
                crate::with_gpu!(gpu, |shell_renderer| {
                    match shell_renderer.import_dmabuf(&pending.buffer, None) {
                        Ok(texture) => {
                            // Once. "The shell did not appear" has two causes
                            // that look identical in the log otherwise: WebKit
                            // never painted, or it painted and the frame was
                            // not drawn.
                            if first {
                                tracing::info!(
                                    "shell {page}: first frame imported, {}x{}",
                                    size.w,
                                    size.h
                                );
                            }
                            match self.shells[page].owned.take() {
                                // Only into a buffer the frame actually fits.
                                // The allocation above failed if this does not
                                // match, and copying anyway would paint a new
                                // frame into part of an old one — a torn
                                // composite of two layouts, which reads as a
                                // rendering bug rather than as the allocation
                                // failure it is.
                                Some((mut buffer, at)) if at == size => {
                                    if let Err(e) = crate::dump::copy_texture(
                                        shell_renderer,
                                        &texture,
                                        &mut buffer,
                                        at,
                                    ) {
                                        tracing::error!("could not copy the shell's frame: {e:#}");
                                    }
                                    // Whichever renderer draws this output
                                    // imports it itself — see `render::build`.
                                    self.shells[page].owned = Some((buffer, at));
                                }
                                Some(kept) => {
                                    tracing::warn!(
                                        "keeping the shell's last frame; this one has nowhere to go"
                                    );
                                    self.shells[page].owned = Some(kept);
                                }
                                None => {
                                    tracing::error!("no image to copy the shell's frame into")
                                }
                            }
                        }
                        Err(e) => tracing::error!("could not import the shell's frame: {e}"),
                    }
                });
            }

            // What WebKit actually painted, once, before anything else can
            // have touched it — the one thing the log cannot say, and the
            // difference between an empty right half and a right half put on
            // screen wrongly. Vulkan only: it is a diagnostic for the renderer
            // that has colour management, and teaching it a second one buys
            // nothing. Re-imported rather than threaded out of the body above,
            // because it runs on the first frame of a session that asked for
            // it and nowhere else.
            if first {
                if let (Some(path), Some(crate::udev::Gpu::Vulkan(vulkan))) =
                    (crate::dump::target(), renderer.as_mut())
                {
                    match vulkan.import_dmabuf(&pending.buffer, None) {
                        Ok(texture) => {
                            if let Err(e) = crate::dump::shell_frame(vulkan, &texture, &path) {
                                tracing::error!("could not dump the shell's frame: {e:#}");
                            }
                        }
                        Err(e) => tracing::error!("could not import for the dump: {e}"),
                    }
                }
            }
            self.shell_renderer = renderer;

            {
                let shell = &self.shells[page].engine;
                // Both, immediately, and in this order.
                //
                // Acknowledging advances WebKit's frame clock; releasing puts
                // the buffer back in its pool. Holding the buffer until the
                // next frame arrives sounds safer and deadlocks instead:
                // WebKit needs a free buffer to paint the next frame, so the
                // frame that would trigger the release can never be painted
                // and the shell stops dead after exactly one.
                //
                // Releasing straight away is safe because the frame has been
                // copied into a buffer of the compositor's own just above. A
                // dup'd fd would not have been enough: it is the same memory,
                // so WebKit would paint into the picture on screen.
                shell.frame_done(&pending.token);
                shell.frame_release(pending.token);
            }
            self.shell_frames += 1;
            tracing::debug!("shell frame {} released", self.shell_frames);
        }

        let shell = &self.shells[page].engine;
        // Frames the mailbox threw away before anything drew them.
        for token in shell.take_stale() {
            shell.frame_release(token);
        }
    }

    /// Bring the shell back after WebKit's web process died.
    ///
    /// The web process is not the compositor's, so its death is survivable —
    /// but nothing recovers on its own. WebKit leaves the view blank and stops
    /// painting, and on a desktop whose entire UI is that view the result is
    /// indistinguishable from a compositor that has hung: the last frame stays
    /// on screen forever and no click does anything.
    ///
    /// The last painted frame is deliberately left up while the reload runs.
    /// It is the compositor's own copy, not WebKit's memory, so it is safe to
    /// keep, and a transient crash then costs a second of a stale bar rather
    /// than a black screen. It is cleared only when recovery is given up on,
    /// where a frozen picture would be a lie about the state of the desktop.
    pub fn restart_shell(&mut self, page: usize, reason: viewport_web::webkit::Termination) {
        use crate::shell::Recovery;

        if self.shells.get(page).is_none() {
            return;
        }
        if !reason.is_recoverable() {
            tracing::warn!("not restarting shell {page}: {reason}");
            return;
        }

        // Per page, not per session. One page crashing says nothing about the
        // health of the other, and a shared budget would let a site that
        // reloads badly use up the desktop's attempts.
        let attempt = {
            let shell = &mut self.shells[page];
            crate::shell::budget(
                &mut shell.restarts,
                &mut shell.restart_window,
                std::time::Instant::now(),
            )
        };

        let attempt = match attempt {
            Recovery::Restart(attempt) => attempt,
            Recovery::GiveUp(count) => {
                // The page is gone either way; what stopping preserves is a
                // machine that can still be logged into and read the log.
                tracing::error!(
                    "shell {page} has died {count} times in {:?}; giving up",
                    crate::shell::RESTART_WINDOW
                );
                // Dropping the copy takes the page out of the element list,
                // and the damage tracker repaints what it covered because the
                // element it knew is gone. Nothing has to be added to its
                // damage bag: that is only read while there is a buffer to
                // describe.
                self.shells[page].owned = None;
                self.needs_render = true;
                return;
            }
        };

        tracing::warn!("restarting shell {page} after {reason} (attempt {attempt})");

        // The new process is a fresh page: it has painted nothing, said
        // nothing, and knows nothing about the layout. Everything derived from
        // the old one has to go with it, or the log claims a shell that is
        // talking and painting while the screen shows neither.
        self.shell_frames = 0;
        self.shell_announced = false;
        self.shells[page].announced = false;
        self.shells[page].size = None;

        match self.shells[page].engine.restart() {
            // Unconditionally, because the size was just cleared: WebKit paints
            // nothing into a view of no size, and a restarted process that is
            // never told its size loads the page and then sits there.
            Ok(()) => self.resize_shell(),
            Err(e) => tracing::error!("could not restart shell {page}: {e:#}"),
        }
    }

    /// Tell the shell how big it is.
    ///
    /// WebKit paints nothing into a view with no size, so without this the
    /// page loads, runs, talks to the compositor — and never produces a frame.
    pub fn resize_shell(&mut self) {
        let (width, height) = self.layout_size();
        if width == 0 || height == 0 {
            return;
        }
        // What the screens now imply, which for a `--url` session can be a
        // different *number* of pages as well as different sizes — see
        // `sync_shells`.
        self.sync_shells();

        for at in 0..self.shells.len() {
            let size = {
                let region = self.shells[at].region;
                (region.size.w.max(0) as u32, region.size.h.max(0) as u32)
            };
            if size.0 == 0 || size.1 == 0 {
                continue;
            }
            // Only on a change: this is called from notify_output_layout, which
            // runs for anything that touches the layout — including a layer
            // surface arriving — and telling WebKit to resize to the size it
            // already has costs a full repaint.
            if self.shells[at].size == Some(size) {
                continue;
            }
            self.shells[at].size = Some(size);
            tracing::info!(
                "shell {at} is {}x{} now, for {}",
                size.0,
                size.1,
                self.space
                    .outputs()
                    .map(|output| {
                        let geometry = self.space.output_geometry(output).unwrap_or_default();
                        format!(
                            "{} {}x{}{:+}{:+} {:?}",
                            output.name(),
                            geometry.size.w,
                            geometry.size.h,
                            geometry.loc.x,
                            geometry.loc.y,
                            output.current_transform()
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            self.shells[at].engine.resize(size.0, size.1);
        }
    }

    /// Start or stop pages so that what is running matches what the screens
    /// call for, and move the ones that stay.
    ///
    /// The out-of-process twin of this is `sync_shell_processes`, and the rule
    /// is the same: reconcile by the document each page is showing rather than
    /// rebuilding, so plugging a monitor into a `--url` session resizes the
    /// page that was already up instead of reloading it.
    ///
    /// A page that has to be *started* here cannot be: `Shell::start` needs the
    /// DRM nodes and the importable format list, which live in the backend and
    /// not in this state. So a plan that calls for one is reported and the
    /// pages that exist are placed as well as they can be — the desktop keeps
    /// the whole layout, which is what it had before the monitor arrived.
    fn sync_shells(&mut self) {
        if self.shells.is_empty() {
            return;
        }
        let planned = self.plan_shells();
        if planned.len() != self.shells.len() {
            tracing::warn!(
                "the screens now call for {} page(s) and {} are running; the in-process engine \
                 cannot start one after the session has begun, so the layout is unchanged",
                planned.len(),
                self.shells.len()
            );
            return;
        }
        for (page, plan) in self.shells.iter_mut().zip(planned) {
            if page.url != plan.url {
                // The plan is positional and both entries are running, so this
                // is the page and the desktop having swapped places, which
                // nothing produces today.
                tracing::warn!("shell plan changed under a running page; leaving it where it is");
                continue;
            }
            page.region = plan.region;
            page.desktop = plan.desktop;
        }
    }
}
