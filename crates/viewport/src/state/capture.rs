// SPDX-License-Identifier: GPL-3.0-or-later
//
// Hit testing and the output/window image capture paths.
// Included as associated items of `ViewportState` by `state.rs`.

impl ViewportState {
    /// What the pointer is over.
    ///
    /// Falls through to nothing when no window is under it, which in the
    /// finished compositor means the shell's own buffer — that is the property
    /// that makes "click went to the titlebar" versus "click went to the app"
    /// need no geometry bookkeeping.
    pub fn surface_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        if self.locked {
            // Only the lock screen may be reached.
            //
            // An external locker's surface is focused explicitly when it
            // commits and is never picked by position, because there is
            // nothing else the pointer may touch. The shell's own lock screen
            // *is* picked by position, because it is an ordinary client
            // surface and this is the only way a finger reaches it — and it is
            // the whole point of drawing the lock screen here that a
            // touch-only desk can use it.
            //
            // Answered by `lock_screen_is_drawing` rather than by the mode
            // alone, which is the fail-closed half: a page that has not said
            // it drew the lock screen is not on screen either, and handing the
            // pointer to a surface nobody can see is a click landing on
            // whatever the page happens to be showing.
            if self.lock_mode.is_built_in() && self.lock_screen_is_drawing() {
                return self.shell_under(pos);
            }
            return None;
        }
        if self.overview {
            // Every click belongs to the shell while it is drawing miniatures.
            return self.shell_under(pos);
        }
        if crate::pointer::over_overlay(&self.shell_overlay_hits, pos) {
            // The shell drew something here in front of the windows — a
            // notification, a floating bar, the screen-share chooser. It is on
            // top, so it takes the pointer; reporting the window underneath
            // would hand the click straight through it.
            return self.shell_under(pos);
        }

        // Layer surfaces first where they are in front, and last where they
        // are behind, so a launcher over a window takes the click and a
        // wallpaper client under one does not.
        let output = self
            .space
            .output_under(pos)
            .next()
            .cloned()
            .or_else(|| self.space.outputs().next().cloned());
        let (above, below) = match output.as_ref() {
            Some(output) => {
                let geometry = self.space.output_geometry(output).unwrap_or_default();
                let local = pos - geometry.loc.to_f64();
                let map = smithay::desktop::layer_map_for_output(output);
                let hit = |layer: Option<&smithay::desktop::LayerSurface>| {
                    let layer = layer?;
                    let at = map.layer_geometry(layer)?.loc.to_f64() + geometry.loc.to_f64();
                    layer
                        .surface_under(pos - at, WindowSurfaceType::ALL)
                        .map(|(s, p)| (s, p.to_f64() + at))
                };
                use smithay::wayland::shell::wlr_layer::Layer;
                (
                    hit(map.layer_under(Layer::Overlay, local))
                        .or_else(|| hit(map.layer_under(Layer::Top, local))),
                    hit(map.layer_under(Layer::Bottom, local))
                        .or_else(|| hit(map.layer_under(Layer::Background, local))),
                )
            }
            None => (None, None),
        };

        if above.is_some() {
            return above;
        }
        // Every window, topmost first, asked directly rather than through
        // `Space::element_under`.
        //
        // That helper finds a window whose own rectangle contains the point,
        // and a menu overflows the window that opened it — so a click on the
        // part of a Firefox menu hanging past the window edge found nothing
        // and went to whatever was behind, which is a menu that cannot be
        // used. `Window::surface_under` looks through the popups as well.
        //
        // Walked in place, back to front. This ran through a `Vec` of cloned
        // `Window`s built and thrown away on every call — and it is called
        // twice for every pointer motion, so a 1000Hz mouse cloned every
        // window on the desktop two thousand times a second to look at each of
        // them once. `Space::elements()` reverses on its own; nothing in the
        // loop touches the `Space` mutably, so there was never anything to
        // borrow around.
        for window in self.space.elements().rev() {
            let Some(location) = self.space.element_location(window) else {
                continue;
            };
            // Not the part of it that is cropped away. See `clipped_out`.
            if self.clipped_out(window, pos) {
                continue;
            }
            // Where the surface is drawn, not where the window is mapped.
            //
            // A client with client-side decorations draws its shadows outside
            // the window: xdg_surface.geometry marks the real window inside a
            // larger surface, and its origin is frequently negative. The map
            // location is the window's, so surface-local coordinates have to
            // start from the surface's — which is what `Space::element_under`
            // returns and what reading the map location instead got wrong, by
            // exactly the width of the shadow.
            // And not the part of it that is merely drawn smaller than it is.
            // The client was never resized, so the coordinates it is asked
            // about are its own; without this a click on a window at 0.5 lands
            // at twice its distance from the corner, which is a pointer that
            // works in the top-left of a window and misses by more the further
            // across it you go.
            let unscaled = self.unscaled(window, pos);
            let render_location = location - window.geometry().loc;
            if let Some((surface, at)) =
                window.surface_under(unscaled - render_location.to_f64(), WindowSurfaceType::ALL)
            {
                // What comes back is not the surface's origin, it is whatever
                // makes the *subtraction* right.
                //
                // The pointer works the surface-local position out by taking
                // this away from the real pointer position, and the real
                // pointer position is in screen coordinates while the point
                // the client should be told about is in its own. Returning the
                // surface's actual origin mixes the two: the window is found
                // correctly and then handed a coordinate off by the whole
                // scale error, which is zero at the window's corner and grows
                // across it — a client where the top-left works and nothing
                // else quite does.
                //
                // So the local point is worked out here, in the window's own
                // coordinates where it means something, and what is returned
                // is the position that yields it. At 1.0 this is exactly
                // `at + render_location`, which is what it always was.
                let local = unscaled - render_location.to_f64() - at.to_f64();
                return Some((surface, pos - local));
            }
        }
        below.or_else(|| self.shell_under(pos))
    }

    /// The topmost window at a point, skipping the parts cropped away.
    ///
    /// `Space::element_under` answers from the mapped rectangles alone, which
    /// on a scrolled strip includes columns that are on a monitor without
    /// being drawn there — see `clipped_out`. Clicking through to whatever is
    /// really underneath is what this adds.
    pub fn window_under(&self, pos: Point<f64, Logical>) -> Option<smithay::desktop::Window> {
        use smithay::desktop::space::SpaceElement;

        // Otherwise as `Space::element_under`: the bounding box rather than the
        // window's own rectangle, so a client's shadow is still part of it, and
        // then the input region, so the parts it says are not clickable are not.
        self.space
            .elements()
            .rev()
            .find(|window| {
                if self.clipped_out(window, pos) {
                    return false;
                }
                // In the window's own coordinates, so that a window merely
                // drawn smaller is tested against the space it actually
                // covers. The bounding box is the full size the `Space` holds
                // it at; against the raw pointer position it claims the screen
                // around a thumbnail as well as the thumbnail.
                let pos = self.unscaled(window, pos);
                let Some(bbox) = self.space.element_bbox(window) else {
                    return false;
                };
                if !bbox.to_f64().contains(pos) {
                    return false;
                }
                // Where the surface is drawn, not where the window is mapped —
                // the same correction `surface_under` makes below.
                let Some(location) = self.space.element_location(window) else {
                    return false;
                };
                let render_location = location - window.geometry().loc;
                window.is_in_input_region(&(pos - render_location.to_f64()))
            })
            .cloned()
    }

    /// Whether a point falls on the part of a window that is cropped away.
    ///
    /// The shell scrolls a strip by moving its columns, not by hiding them: a
    /// column scrolled off the left of one monitor keeps a rectangle, and that
    /// rectangle lands on the monitor beside it. Nothing of it is *drawn*
    /// there — `view.layout` carries a clip and the renderer crops the surface
    /// to it — but the window is still mapped in the `Space` at its full size,
    /// so every hit test found it. With the second monitor scrolled a few
    /// columns along, clicking a window on the first monitor focused an
    /// invisible column of the second instead, and the strip scrolled back to
    /// it: the click had gone to a window that is not on that screen.
    ///
    /// So the clip bounds input as well as drawing. The two have to agree —
    /// what is not on the screen cannot be clicked — and the clip is the only
    /// thing that knows where a window really is.
    /// What the shell asked this window to be *drawn* at, 1.0 for almost
    /// everything.
    fn draw_scale(&self, window: &smithay::desktop::Window) -> f64 {
        use smithay::wayland::seat::WaylandFocus;

        window
            .wl_surface()
            .as_deref()
            .and_then(|surface| self.views.find_by_surface(surface))
            .map(|view| view.scale)
            .filter(|scale| scale.is_finite() && *scale > 0.0)
            .unwrap_or(1.0)
    }

    /// A point on the screen, in the coordinates of a window drawn smaller than
    /// it is.
    ///
    /// A shrunken window is a client that has not been resized: it is painted
    /// at its own size and the renderer scales the result about the window's
    /// top-left corner (`WindowFrame::origin`, and `RescaleRenderElement` in
    /// render.rs). The `Space` still holds it at full size, because that is
    /// what it is — so every hit test asked "which window is under the
    /// pointer" of a layout drawn at one scale and stored at another, and got
    /// an answer for a window that is not what anybody can see.
    ///
    /// Undoing the scale about the same corner is the whole correction. It is
    /// identity at 1.0, which is every window in every layout that does not
    /// shrink one, so the arithmetic is skipped rather than rounded through.
    ///
    /// The corner is `element_geometry().loc`: the window's own top-left, not
    /// the surface's — a client drawing its shadows outside its geometry starts
    /// its surface some pixels up and left of the window, and scaling about
    /// *that* is what leaves a strip of window hanging outside the box the
    /// shell drew. The renderer picks the same corner for the same reason.
    pub fn unscaled(
        &self,
        window: &smithay::desktop::Window,
        pos: Point<f64, Logical>,
    ) -> Point<f64, Logical> {
        let scale = self.draw_scale(window);
        if (scale - 1.0).abs() < f64::EPSILON {
            return pos;
        }
        let Some(origin) = self
            .space
            .element_geometry(window)
            .map(|geometry| geometry.loc)
        else {
            return pos;
        };
        unscale_about(origin.to_f64(), pos, scale)
    }

    pub fn clipped_out(&self, window: &smithay::desktop::Window, pos: Point<f64, Logical>) -> bool {
        use smithay::wayland::seat::WaylandFocus;

        let Some(clip) = window
            .wl_surface()
            .as_deref()
            .and_then(|surface| self.views.find_by_surface(surface))
            .and_then(|view| view.clip)
        else {
            // No clip means nothing was cropped: the whole window is on
            // screen, which is every window on an unscrolled workspace.
            return false;
        };
        /* The clip is in the window's own coordinates — the shell divides the
        thumbnail scale back out before sending it — so a point on a shrunken
        window has to come back the same way before it is compared. */
        let pos = self.unscaled(window, pos);
        !crate::views::clip_covers(clip, pos.x, pos.y)
    }

    /// The shell's own surface at a point, for the out-of-process backend.
    ///
    /// This is the whole of its input handling. Where the WPE backend answers
    /// `None` — "the pointer is on the shell, so tell the engine directly" —
    /// this answers with a surface, and the pointer and keyboard take it from
    /// there exactly as they would for any other client. A click on a titlebar
    /// the shell drew is a `wl_pointer.button` on the shell's surface.
    ///
    /// One buffer across the whole layout, mapped at the layout's origin, so a
    /// position in layout coordinates is already surface-local.
    fn shell_under(&self, pos: Point<f64, Logical>) -> Option<(WlSurface, Point<f64, Logical>)> {
        // Where the *surface* is, not where the pointer is: smithay subtracts
        // this from the pointer's position to get the surface-local
        // coordinate. Returning the pointer's own position made every click
        // arrive at (0, 0) — the top-left corner of the page, whatever had
        // been aimed at — which is why nothing the shell drew could be
        // clicked.
        //
        // The corner of whichever page is under the pointer, not the layout's
        // origin: a desktop on its own is mapped at the origin and the two are
        // the same, and a `--url` page on the second monitor is not.
        self.shell_at(pos)
    }

    /// Advertise linux-dmabuf, with the formats this renderer can import.
    ///
    /// After the backend, not before: the format list is the renderer's, and a
    /// global advertising formats nobody can import is worse than none — the
    /// client picks one, hands over a buffer, and finds out at the first frame.
    ///
    /// The feedback names the render node, which is how a client knows which
    /// GPU to allocate on when there is more than one.
    pub fn advertise_dmabuf(
        &mut self,
        render_node: Option<u64>,
        formats: Vec<smithay::backend::allocator::Format>,
    ) {
        use smithay::wayland::dmabuf::DmabufFeedbackBuilder;

        if formats.is_empty() {
            tracing::warn!("the renderer imports no dmabuf format; not advertising linux-dmabuf");
            return;
        }
        let Some(node) = render_node else {
            tracing::warn!("no render node; not advertising linux-dmabuf");
            return;
        };

        let count = formats.len();
        match DmabufFeedbackBuilder::new(node, formats).build() {
            Ok(feedback) => {
                self.dmabuf_state
                    .create_global_with_default_feedback::<Self>(&self.display_handle, &feedback);
                tracing::info!("linux-dmabuf: {count} format/modifier pair(s)");
            }
            Err(e) => tracing::error!("could not build dmabuf feedback: {e}"),
        }
    }

    /// Copy every frame waiting on `output`, and answer its client.
    ///
    /// Generic over the renderer because the two backends have different ones
    /// and neither is reachable from where the request arrives: the nested
    /// backend's lives inside its event loop. A backend calls this while it
    /// holds its renderer, right after it has drawn.
    ///
    /// Composited fresh rather than read back from the scanout buffer: the
    /// front buffer holds whatever was last flipped, which for an idle screen
    /// is a frame of unknown age — for a screenshot that is the difference
    /// between the current desktop and one from a minute ago.
    pub fn service_screencopy<R, B>(&mut self, output: &Output, renderer: &mut R)
    where
        R: Renderer
            + Bind<B>
            + Offscreen<B>
            + ExportMem
            + smithay::backend::renderer::ImportAll
            + smithay::backend::renderer::ImportMem
            + smithay::backend::renderer::ImportDma,
        // Held between frames; see `capture_scratch`.
        B: 'static,
        <R as smithay::backend::renderer::RendererSuper>::TextureId: Clone + Send + Sync + 'static,
        <R as smithay::backend::renderer::RendererSuper>::Error: Send + Sync + 'static,
    {
        if self.pending_copies.is_empty() {
            return;
        }
        // Only this output's. A second monitor's copies wait for that monitor
        // to draw, which is where its renderer will be.
        let mut mine = Vec::new();
        self.pending_copies.retain(|copy| {
            if copy.output == *output {
                mine.push(copy.clone());
                false
            } else {
                true
            }
        });

        for copy in mine {
            // The client went away between asking and being served, which is
            // ordinary: a screenshot tool that was killed mid-copy.
            if !copy.frame.is_alive() {
                continue;
            }
            match self.copy_one(output, &copy, renderer) {
                Ok(()) => crate::screencopy::finish(&copy.frame, copy.region, copy.with_damage),
                Err(e) => {
                    tracing::warn!("screencopy failed: {e}");
                    copy.frame.failed();
                }
            }
        }

        self.service_portal_screenshots::<R, B>(output, renderer);
    }

    /// Serve pending portal screenshot requests on `output`.
    pub fn service_portal_screenshots<R, B>(&mut self, output: &Output, renderer: &mut R)
    where
        R: Renderer
            + Bind<B>
            + Offscreen<B>
            + ExportMem
            + smithay::backend::renderer::ImportAll
            + smithay::backend::renderer::ImportMem
            + smithay::backend::renderer::ImportDma,
        B: 'static,
        <R as smithay::backend::renderer::RendererSuper>::TextureId: Clone + Send + Sync + 'static,
        <R as smithay::backend::renderer::RendererSuper>::Error: Send + Sync + 'static,
    {
        if self.pending_screenshots.is_empty() {
            return;
        }

        let mut mine = Vec::new();
        self.pending_screenshots.retain(|req| {
            if req.output.as_ref() == Some(output)
                || (req.output.is_none() && self.space.outputs().next() == Some(output))
            {
                mine.push(req.reply.clone());
                false
            } else {
                true
            }
        });

        for reply in mine {
            let mode = output
                .current_mode()
                .unwrap_or_else(|| smithay::output::Mode {
                    size: (1920, 1080).into(),
                    refresh: 60_000,
                });
            let region = smithay::utils::Rectangle::new((0, 0).into(), mode.size);
            match self.read_output_pixels::<R, B>(output, region, true, renderer) {
                Ok(pixels) => {
                    // The encoding and the writing belong to nobody's frame:
                    // PNG over a full screen is tens of milliseconds at 1080p
                    // and hundreds at 4K, and this runs between frames. A
                    // short-lived thread does both and answers the portal from
                    // there — once, whichever way it goes, because exactly one
                    // `try_send` sits on each path out of it.
                    let size = mode.size;
                    // Named here rather than in the thread, so the path is
                    // written down for the housekeeping tick before anything
                    // can fail: the file is taken back either way. See
                    // `screenshot_temp_path` for why it lives where it does,
                    // and `reap_screenshot_temps` for how it leaves.
                    let path = screenshot_temp_path();
                    self.screenshot_temps
                        .push((path.clone(), std::time::Instant::now()));
                    let unspawned = reply.clone();
                    let spawned = std::thread::Builder::new()
                        .name("viewport-screenshot".to_owned())
                        .spawn(move || {
                            let png_bytes =
                                crate::icon::encode_png(size.w as u32, size.h as u32, &pixels);
                            // `create_new`, not write: a name somebody else
                            // planted — this one is predictable by the pid and
                            // the clock alone — is an error to dodge rather
                            // than a symlink to follow, and the 0600 the file
                            // is born with is one less thing to race.
                            let written = std::fs::OpenOptions::new()
                                .write(true)
                                .create_new(true)
                                .mode(0o600)
                                .open(&path)
                                .and_then(|mut file| {
                                    std::io::Write::write_all(&mut file, &png_bytes)
                                });
                            match written {
                                Ok(()) => {
                                    let uri = format!("file://{}", path.display());
                                    let _ = reply.try_send(Ok(uri));
                                }
                                Err(e) => {
                                    let _ = reply.try_send(Err(format!(
                                        "could not write screenshot file: {e}"
                                    )));
                                }
                            }
                        });
                    // A thread that would not start still leaves a request to
                    // answer, so the portal hears the failure rather than
                    // waiting on nothing.
                    if let Err(e) = spawned {
                        let _ = unspawned
                            .try_send(Err(format!("could not spawn the screenshot writer: {e}")));
                    }
                }
                Err(e) => {
                    let _ = reply.try_send(Err(e));
                }
            }
        }
    }

    /// Take back the screenshot files that have been handed out long enough.
    ///
    /// The portal reply is a URI, and nothing tells this end when the client
    /// has finished reading the file it names — so a grace period stands in
    /// for the answer, and the file goes once it has passed. Without this
    /// every screenshot ever taken lived for the rest of the session; under
    /// the runtime directory they are private, but they are still disk.
    pub fn reap_screenshot_temps(&mut self) {
        // Long enough for the portal to answer and the application to open
        // what it was handed, short enough that a session taking screenshots
        // in a loop does not pile up a minute of them.
        const GRACE: std::time::Duration = std::time::Duration::from_secs(60);
        self.screenshot_temps.retain(|(path, written)| {
            if written.elapsed() < GRACE {
                return true;
            }
            match std::fs::remove_file(path) {
                Ok(()) => false,
                // Already gone is gone. Anything else — a directory replaced
                // by something odd, a filesystem objecting — is said once and
                // not retried: keeping the entry would mean saying it again
                // every second for the life of the session.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
                Err(e) => {
                    tracing::warn!(
                        "could not take back the screenshot at {}: {e}",
                        path.display()
                    );
                    false
                }
            }
        });
    }

    /// Drop screencopy requests that can no longer be served.
    ///
    /// `service_screencopy` runs only while the output each request names is
    /// being drawn, so an entry whose output has been unplugged or switched
    /// off since the ask — or whose asking client has died — is never reached
    /// by it, and sat here holding its `wl_buffer` and whatever shared memory
    /// is behind it for the rest of the session. This is the same sweep the
    /// housekeeping tick gives everything else that outlives its owner.
    pub fn reap_pending_copies(&mut self) {
        let before = self.pending_copies.len();
        self.pending_copies.retain(|copy| {
            if !copy.frame.is_alive() {
                return false;
            }
            // Not in the space any more: unplugged, or switched off through
            // output management. No frame will come from either.
            if !self.space.outputs().any(|other| other == &copy.output) {
                // The frame is still alive, so say why it will not be served:
                // dropping it in silence leaves the client waiting on `ready`
                // for ever, which is exactly the state this reaps.
                copy.frame.failed();
                return false;
            }
            true
        });
        if self.pending_copies.len() != before {
            tracing::debug!(
                "reaped {} screencopy request(s) whose output or client is gone",
                before - self.pending_copies.len()
            );
        }
    }

    /// Drop every pending screencopy for one output, telling their clients.
    ///
    /// The half of [`Self::reap_pending_copies`] that runs the moment an
    /// output goes away rather than a second later.
    fn drop_pending_copies_for(&mut self, output: &Output) {
        let before = self.pending_copies.len();
        self.pending_copies.retain(|copy| {
            if copy.output != *output {
                return true;
            }
            if copy.frame.is_alive() {
                // Said rather than dropped in silence: a client waiting on
                // `ready` for ever is the state all of this exists to prevent.
                copy.frame.failed();
            }
            false
        });
        if self.pending_copies.len() != before {
            tracing::debug!(
                "dropped {} screencopy request(s) for {}, which is off",
                before - self.pending_copies.len(),
                output.name()
            );
        }
    }

    /// Serve every capture frame waiting on `output`.
    ///
    /// The same arrangement as screencopy: the copy happens where the renderer
    /// is, which is inside a backend, so the request only queues.
    pub fn service_image_capture<R, B>(&mut self, output: &Output, renderer: &mut R)
    where
        R: Renderer
            + Bind<B>
            + Bind<smithay::backend::allocator::dmabuf::Dmabuf>
            + Offscreen<B>
            + ExportMem
            + smithay::backend::renderer::ImportAll
            + smithay::backend::renderer::ImportMem
            + smithay::backend::renderer::ImportDma,
        // Held between frames; see `capture_scratch`.
        B: 'static,
        <R as smithay::backend::renderer::RendererSuper>::TextureId: Clone + Send + Sync + 'static,
        <R as smithay::backend::renderer::RendererSuper>::Error: Send + Sync + 'static,
    {
        if self.pending_capture_frames.is_empty() {
            return;
        }
        // A window's frame is served on the pass for a screen it is on, the
        // same rule the screencast path uses: this runs once per output and a
        // window has to be picked up by exactly one of those passes, or it is
        // either drawn twice or never.
        let mut mine = Vec::new();
        let mut rest = Vec::new();
        for (target, frame) in std::mem::take(&mut self.pending_capture_frames) {
            let ours = match &target {
                CaptureTarget::Output(frame_output) => frame_output == output,
                CaptureTarget::Window(id) => self.window_is_on(*id, output),
            };
            if ours {
                mine.push((target, frame));
            } else {
                rest.push((target, frame));
            }
        }
        self.pending_capture_frames = rest;

        let mut windows = Vec::new();
        let mut outputs = Vec::new();
        for (target, frame) in mine {
            match target {
                CaptureTarget::Output(frame_output) => outputs.push((frame_output, frame)),
                CaptureTarget::Window(id) => windows.push((id, frame)),
            }
        }

        for (id, frame) in windows {
            let buffer = frame.buffer();
            let result = match smithay::wayland::dmabuf::get_dmabuf(&buffer) {
                Ok(dmabuf) => self.render_window_into(id, dmabuf.clone(), renderer),
                Err(_) => self.copy_window_into::<R, B>(id, &buffer, renderer),
            };
            match result {
                Ok(()) => {
                    tracing::debug!("image capture: a frame of view {id}");
                    let now = self.start_time.elapsed();
                    // Normal, unlike an output. A window is not rotated by the
                    // screen it happens to be on — `read_window_pixels` draws
                    // it upright — so telling a client the screen's transform
                    // would have it turn an already-upright picture.
                    frame.success(smithay::utils::Transform::Normal, None, now);
                }
                Err(e) => {
                    tracing::warn!("image capture of view {id} failed: {e}");
                    frame.fail(
                        smithay::reexports::wayland_protocols::ext::image_copy_capture::v1::server::ext_image_copy_capture_frame_v1::FailureReason::Unknown,
                    );
                }
            }
        }

        for (frame_output, frame) in outputs {
            let size = frame_output
                .current_mode()
                .map(|mode| frame_output.current_transform().transform_size(mode.size))
                .unwrap_or_default();
            let region = smithay::utils::Rectangle::from_size((size.w, size.h).into());
            let buffer = frame.buffer();
            // The cursor is a separate session in this protocol — a client
            // that wants it asks for it — so the copy of the output has none
            // in it.
            //
            // A dmabuf is drawn into directly. That is the whole reason a
            // recorder wants this protocol: the shared-memory path reads every
            // pixel back across the bus for each frame, which is affordable
            // once for a screenshot and not sixty times a second for a video.
            let result = match smithay::wayland::dmabuf::get_dmabuf(&buffer) {
                Ok(dmabuf) => {
                    self.render_output_into(&frame_output, dmabuf.clone(), false, renderer)
                }
                Err(_) => {
                    self.copy_output_into::<R, B>(&frame_output, region, false, &buffer, renderer)
                }
            };
            match result {
                Ok(()) => {
                    // Debug, not info: a recorder asks sixty times a second.
                    tracing::debug!("image capture: a frame of {}", frame_output.name());
                    let now = self.start_time.elapsed();
                    // The output's own transform, not Normal. The copy is
                    // composited the way the output is composited, so a client
                    // that is told Normal on a rotated or flipped monitor
                    // writes out an upside-down picture — which is exactly
                    // what the nested backend, whose output is flipped,
                    // produced.
                    //
                    // No damage: composited fresh, so the whole thing is new.
                    frame.success(frame_output.current_transform(), None, now);
                }
                Err(e) => {
                    tracing::warn!("image capture failed: {e}");
                    frame.fail(
                        smithay::reexports::wayland_protocols::ext::image_copy_capture::v1::server::ext_image_copy_capture_frame_v1::FailureReason::Unknown,
                    );
                }
            }
        }
    }

    /// Composite an output straight into a buffer the client allocated.
    ///
    /// No readback: the frame is rendered where it is going to be read from,
    /// which is what makes recording at a screen's refresh rate possible at
    /// all.
    fn render_output_into<R>(
        &mut self,
        output: &Output,
        mut target: smithay::backend::allocator::dmabuf::Dmabuf,
        overlay_cursor: bool,
        renderer: &mut R,
    ) -> Result<(), String>
    where
        R: Renderer
            + Bind<smithay::backend::allocator::dmabuf::Dmabuf>
            + smithay::backend::renderer::ImportAll
            + smithay::backend::renderer::ImportMem
            + smithay::backend::renderer::ImportDma,
        <R as smithay::backend::renderer::RendererSuper>::TextureId: Clone + Send + Sync + 'static,
        <R as smithay::backend::renderer::RendererSuper>::Error: Send + Sync + 'static,
    {
        let source = self.mirror_source(output);
        let mut frame = self.frame_for(&source);
        if !overlay_cursor {
            frame.cursor = crate::render::Cursor::Hidden;
        }

        let size = output
            .current_mode()
            .map(|mode| output.current_transform().transform_size(mode.size))
            .ok_or_else(|| "the output has no mode".to_owned())?;
        let elements = crate::render::build(&frame, renderer);

        let mut framebuffer = renderer
            .bind(&mut target)
            .map_err(|e| format!("binding the client's buffer: {e}"))?;
        // From the output, so a rotated screen is drawn into the client's
        // buffer the way it is displayed rather than the way it is laid out.
        let mut tracker = smithay::backend::renderer::damage::OutputDamageTracker::new(
            size,
            output.current_scale().fractional_scale(),
            smithay::utils::Transform::Normal,
        );
        let result = tracker
            .render_output(
                renderer,
                &mut framebuffer,
                0,
                &elements,
                smithay::backend::renderer::Color32F::from([0.0, 0.0, 0.0, 1.0]),
            )
            .map_err(|e| format!("compositing into the client's buffer: {e:?}"))?;

        // Wait for the GPU to finish before the client is told the frame is
        // ready.
        //
        // Rendering is asynchronous: `render_output` returns once the work is
        // submitted, not once it is done. The shared-memory path reads the
        // result back, which waits by itself; this one hands the client the
        // buffer the GPU is still writing into, so a recorder that reads it
        // immediately sees whatever was there before — an untouched buffer,
        // which is black. That is a screen share of a black rectangle at the
        // right resolution and the right frame rate.
        if let Err(e) = result.sync.wait() {
            return Err(format!("waiting for the capture to finish: {e}"));
        }
        Ok(())
    }

    /// Every monitor at once, as one picture of the desk.
    ///
    /// Built from each output's own frame rather than from a frame of its own:
    /// what belongs on a screen — which layer surfaces, which part of the
    /// shell, where the pointer is — is decided per output and there is no
    /// second answer for the desk. So each is worked out exactly as it is
    /// displayed and then moved into place, which also means a rotated monitor
    /// arrives rotated and a scaled one arrives at the desk's scale.
    ///
    /// The pointer comes out right for free: `cursor_for` draws it only on the
    /// output it is over, so exactly one of these frames carries it.
    fn desk_elements<R>(&mut self, renderer: &mut R) -> Result<DeskElements<R>, String>
    where
        R: Renderer
            + smithay::backend::renderer::ImportAll
            + smithay::backend::renderer::ImportMem
            + smithay::backend::renderer::ImportDma,
        <R as smithay::backend::renderer::RendererSuper>::TextureId: Clone + Send + Sync + 'static,
    {
        use smithay::backend::renderer::element::utils::{
            CropRenderElement, Relocate, RelocateRenderElement, RescaleRenderElement,
        };

        let (union, scale) = self
            .all_outputs_layout()
            .ok_or_else(|| "there are no monitors".to_owned())?;
        let size = self
            .desk_size()
            .ok_or_else(|| "there are no monitors".to_owned())?;

        // Collected first: building a frame needs the whole state, and the
        // space cannot be iterated across that.
        let outputs: Vec<Output> = self.space.outputs().cloned().collect();

        let mut elements: Vec<DeskElement<R>> = Vec::new();
        for output in outputs {
            let Some(geometry) = self.space.output_geometry(&output) else {
                continue;
            };
            let frame = self.frame_for(&output);
            // From the frame rather than from the output, because that is the
            // scale its elements were laid out at.
            let magnify = scale / frame.scale.max(f64::MIN_POSITIVE);
            // Where this monitor's picture goes, and the rectangle it is held
            // to — its own, with the monitor at the origin, because the crop
            // happens before the move. See `render::desk_placement`.
            let (bounds, at) = crate::render::desk_placement(geometry, union, scale);
            elements.extend(
                crate::render::build(&frame, renderer)
                    .into_iter()
                    .filter_map(|element| {
                        let scaled = RescaleRenderElement::from_element(
                            element,
                            smithay::utils::Point::from((0, 0)),
                            magnify,
                        );
                        // Cropped away entirely: an element of this monitor's
                        // frame that belongs to another one, which is most of
                        // the shell every time.
                        let cropped = CropRenderElement::from_element(scaled, scale, bounds)?;
                        Some(RelocateRenderElement::from_element(
                            cropped,
                            at,
                            Relocate::Relative,
                        ))
                    }),
            );
        }

        // Front to back within an output, and in the space's order between
        // them. The order between them does not matter: two monitors do not
        // overlap, so nothing on one is ever in front of anything on another.
        Ok((elements, size))
    }

    /// Composite the whole desk straight into a consumer's buffer.
    fn render_desk_into<R>(
        &mut self,
        mut target: smithay::backend::allocator::dmabuf::Dmabuf,
        renderer: &mut R,
    ) -> Result<(), String>
    where
        R: Renderer
            + Bind<smithay::backend::allocator::dmabuf::Dmabuf>
            + smithay::backend::renderer::ImportAll
            + smithay::backend::renderer::ImportMem
            + smithay::backend::renderer::ImportDma,
        <R as smithay::backend::renderer::RendererSuper>::TextureId: Clone + Send + Sync + 'static,
        <R as smithay::backend::renderer::RendererSuper>::Error: Send + Sync + 'static,
    {
        let (elements, size) = self.desk_elements(renderer)?;

        let mut framebuffer = renderer
            .bind(&mut target)
            .map_err(|e| format!("binding the client's buffer: {e}"))?;
        // Upright and unscaled: every element was already placed in the desk's
        // own physical pixels, including whatever rotation its monitor has.
        let mut tracker = smithay::backend::renderer::damage::OutputDamageTracker::new(
            size,
            1.0,
            smithay::utils::Transform::Normal,
        );
        let result = tracker
            .render_output(
                renderer,
                &mut framebuffer,
                0,
                &elements,
                smithay::backend::renderer::Color32F::from([0.0, 0.0, 0.0, 1.0]),
            )
            .map_err(|e| format!("compositing the desk into the client's buffer: {e:?}"))?;

        // Waited for, because nothing else will: the client is handed the
        // buffer the GPU is still writing into, and a consumer that reads it
        // straight away sees whatever was there before.
        result
            .sync
            .wait()
            .map_err(|e| format!("waiting for the capture to finish: {e}"))
    }

    /// Composite the whole desk and read it back, for a stream that cannot be
    /// drawn into directly.
    fn read_desk_pixels<R, B>(
        &mut self,
        renderer: &mut R,
    ) -> Result<(Vec<u8>, smithay::utils::Size<i32, smithay::utils::Physical>), String>
    where
        R: Renderer
            + Bind<B>
            + Offscreen<B>
            + ExportMem
            + smithay::backend::renderer::ImportAll
            + smithay::backend::renderer::ImportMem
            + smithay::backend::renderer::ImportDma,
        // Held between frames; see `capture_scratch`.
        B: 'static,
        <R as smithay::backend::renderer::RendererSuper>::TextureId: Clone + Send + Sync + 'static,
        <R as smithay::backend::renderer::RendererSuper>::Error: Send + Sync + 'static,
    {
        let (elements, size) = self.desk_elements(renderer)?;

        // The format it will be read back as, because the Vulkan renderer
        // refuses to convert while copying — see `read_output_pixels`.
        let format = smithay::backend::allocator::Fourcc::Xrgb8888;
        let buffer_size: smithay::utils::Size<i32, smithay::utils::Buffer> =
            (size.w, size.h).into();
        let mut target: B = self.take_capture_target(renderer, format, buffer_size)?;

        let mapping = {
            let mut framebuffer = renderer
                .bind(&mut target)
                .map_err(|e| format!("binding a desk capture target: {e}"))?;
            let mut tracker = smithay::backend::renderer::damage::OutputDamageTracker::new(
                size,
                1.0,
                smithay::utils::Transform::Normal,
            );
            tracker
                .render_output(
                    renderer,
                    &mut framebuffer,
                    0,
                    &elements,
                    smithay::backend::renderer::Color32F::from([0.0, 0.0, 0.0, 1.0]),
                )
                .map_err(|e| format!("compositing the desk: {e:?}"))?;
            renderer
                .copy_framebuffer(
                    &framebuffer,
                    smithay::utils::Rectangle::from_size(buffer_size),
                    format,
                )
                .map_err(|e| format!("reading the desk back: {e}"))?
        };
        let pixels = renderer
            .map_texture(&mapping)
            .map_err(|e| format!("mapping a desk capture: {e}"))?
            .to_vec();
        self.keep_capture_target(format, buffer_size, target);
        Ok((pixels, size))
    }

    fn copy_one<R, B>(
        &mut self,
        output: &Output,
        copy: &PendingCopy,
        renderer: &mut R,
    ) -> Result<(), String>
    where
        R: Renderer
            + Bind<B>
            + Offscreen<B>
            + ExportMem
            + smithay::backend::renderer::ImportAll
            + smithay::backend::renderer::ImportMem
            + smithay::backend::renderer::ImportDma,
        // Held between frames; see `capture_scratch`.
        B: 'static,
        <R as smithay::backend::renderer::RendererSuper>::TextureId: Clone + Send + Sync + 'static,
        <R as smithay::backend::renderer::RendererSuper>::Error: Send + Sync + 'static,
    {
        self.copy_output_into::<R, B>(
            output,
            copy.region,
            copy.overlay_cursor,
            &copy.buffer,
            renderer,
        )
    }

    /// Composite `output` and write `region` of it into a client's shared
    /// memory buffer.
    ///
    /// Shared by both capture protocols. They disagree about how a client asks
    /// and how it is told, and not at all about what a screenshot is.
    pub fn copy_output_into<R, B>(
        &mut self,
        output: &Output,
        region: smithay::utils::Rectangle<i32, smithay::utils::Physical>,
        overlay_cursor: bool,
        buffer: &smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer,
        renderer: &mut R,
    ) -> Result<(), String>
    where
        R: Renderer
            + Bind<B>
            + Offscreen<B>
            + ExportMem
            + smithay::backend::renderer::ImportAll
            + smithay::backend::renderer::ImportMem
            + smithay::backend::renderer::ImportDma,
        // Held between frames; see `capture_scratch`.
        B: 'static,
        <R as smithay::backend::renderer::RendererSuper>::TextureId: Clone + Send + Sync + 'static,
        <R as smithay::backend::renderer::RendererSuper>::Error: Send + Sync + 'static,
    {
        let pixels = self.read_output_pixels::<R, B>(output, region, overlay_cursor, renderer)?;

        // Into the client's own memory. The shm path is the only one a client
        // can read without having allocated the buffer itself.
        blit_shm(buffer, &pixels, region.size, "the copy")?;

        Ok(())
    }

    /// Composite an output and read it back, packed, four bytes to a pixel.
    ///
    /// The shared half of every capture that cannot be drawn into directly: a
    /// client's shared memory, and a PipeWire buffer.
    pub fn read_output_pixels<R, B>(
        &mut self,
        output: &Output,
        region: smithay::utils::Rectangle<i32, smithay::utils::Physical>,
        overlay_cursor: bool,
        renderer: &mut R,
    ) -> Result<Vec<u8>, String>
    where
        R: Renderer
            + Bind<B>
            + Offscreen<B>
            + ExportMem
            + smithay::backend::renderer::ImportAll
            + smithay::backend::renderer::ImportMem
            + smithay::backend::renderer::ImportDma,
        // Held between frames; see `capture_scratch`.
        B: 'static,
        <R as smithay::backend::renderer::RendererSuper>::TextureId: Clone + Send + Sync + 'static,
        <R as smithay::backend::renderer::RendererSuper>::Error: Send + Sync + 'static,
    {
        let source = self.mirror_source(output);
        let mut frame = self.frame_for(&source);
        if !overlay_cursor {
            // A screenshot with a pointer in it is rarely what was asked for,
            // and the client says which it wants.
            frame.cursor = crate::render::Cursor::Hidden;
        }

        let size = output
            .current_mode()
            .map(|mode| output.current_transform().transform_size(mode.size))
            .ok_or_else(|| "the output has no mode".to_owned())?;

        let elements = crate::render::build(&frame, renderer);
        // What went into the copy. A capture that comes back black is either a
        // frame with nothing in it or a frame that was drawn and read back
        // wrong, and the picture alone cannot say which.
        tracing::debug!(
            "capture of {}: {} element(s), {} window(s), shell {}",
            output.name(),
            elements.len(),
            frame.windows.len(),
            if frame.shell.is_some() { "yes" } else { "no" }
        );

        let buffer_size: smithay::utils::Size<i32, smithay::utils::Buffer> =
            (size.w, size.h).into();
        // Allocated in the format it will be read back as, because a renderer
        // is entitled to refuse to convert while copying and the Vulkan one
        // does: "cannot convert DrmFourcc(AR24) to DrmFourcc(XR24) while
        // copying" is what every capture on real hardware said, while the
        // nested GLES renderer converted quietly and hid it.
        //
        // XRGB either way, which is what a client is offered: a screenshot has
        // no transparency to carry, and a client that read the fourth byte as
        // alpha would show the whole image as see-through.
        let format = smithay::backend::allocator::Fourcc::Xrgb8888;
        let mut target: B = self.take_capture_target(renderer, format, buffer_size)?;

        let mapping = {
            let mut framebuffer = renderer
                .bind(&mut target)
                .map_err(|e| format!("binding the copy target: {e}"))?;
            // From the output, so the copy carries its scale and its
            // transform. Hand-rolling it as (mode size, 1.0, Normal)
            // composites the desktop in the output's logical space — portrait,
            // for a rotated screen — and writes it into a landscape buffer
            // without turning it, which is a screenshot lying on its side.
            let mut tracker = smithay::backend::renderer::damage::OutputDamageTracker::new(
                size,
                output.current_scale().fractional_scale(),
                smithay::utils::Transform::Normal,
            );
            tracker
                .render_output(
                    renderer,
                    &mut framebuffer,
                    0,
                    &elements,
                    smithay::backend::renderer::Color32F::from([0.0, 0.0, 0.0, 1.0]),
                )
                .map_err(|e| format!("compositing the copy: {e:?}"))?;

            renderer
                .copy_framebuffer(
                    &framebuffer,
                    smithay::utils::Rectangle::new(
                        (region.loc.x, region.loc.y).into(),
                        (region.size.w, region.size.h).into(),
                    ),
                    format,
                )
                .map_err(|e| format!("reading the copy back: {e}"))?
        };
        let pixels = renderer
            .map_texture(&mapping)
            .map_err(|e| format!("mapping the copy: {e}"))?
            .to_vec();
        self.keep_capture_target(format, buffer_size, target);
        Ok(pixels)
    }
}
