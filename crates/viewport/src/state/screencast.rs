// SPDX-License-Identifier: GPL-3.0-or-later
//
// PipeWire cast setup, target selection, rendering and portal handling.
// Included as associated items of `ViewportState` by `state.rs`.

impl ViewportState {
    /// Start sharing an output, and say which PipeWire node to watch.
    ///
    /// The connection is made on the first request rather than at startup: a
    /// desktop nobody is sharing has no reason to hold one open, and a session
    /// without PipeWire should still be a working desktop.
    ///
    /// What comes back is everything the portal's answer is made of, minus
    /// the answer itself: the node does not exist yet — the daemon names the
    /// stream on its own clock, usually within a couple of milliseconds — so
    /// nothing here waits for it. The caller arms the promise that answers
    /// the reply when the name arrives and the deadline that refuses the
    /// share if it never does; see `begin_cast` and `finish_share`.
    fn start_cast(&mut self, source: crate::screencast::Source) -> anyhow::Result<BegunCast> {
        if self.pipewire.is_none() {
            self.pipewire = Some(crate::screencast::stream::Pipewire::new()?);
        }

        // Resolved once here for the name and the first size, and again on
        // every frame after: a following source names whatever is in front
        // now, and what that is will have changed by the second frame.
        let target = self
            .resolve_cast(&source)
            .ok_or_else(|| anyhow::anyhow!("there is nothing to share"))?;
        let name = self.cast_name(&source, &target);
        let size = self
            .target_size(&target)
            .ok_or_else(|| anyhow::anyhow!("{name} cannot be captured"))?;

        // Buffers the GPU can draw into, if this backend can allocate any.
        // Without them the stream falls back to shared memory, which costs a
        // whole screen off the GPU and back for every frame.
        let targets = self.cast_targets(size);
        let pipewire = self.pipewire.as_ref().expect("just connected");
        let stream = pipewire.create_stream(&name, size, targets)?;
        let arrival = stream.arrival();
        let stream_id = stream.id;
        self.casts.push(crate::screencast::Cast { source, stream });
        Ok(BegunCast {
            arrival,
            stream_id,
            size,
            name,
        })
    }

    /// Allocate the buffers a stream will hand out.
    ///
    /// All of them or none: a stream with some of its buffers is one that
    /// stutters between the two paths, and the shared-memory fallback works
    /// whole.
    ///
    /// Only on the DRM backend. It is the one with a GPU allocator, and it is
    /// also the only one anybody shares a screen from — nested and headless
    /// are for testing, and both still stream through shared memory.
    fn cast_targets(
        &mut self,
        size: smithay::utils::Size<i32, smithay::utils::Physical>,
    ) -> Vec<smithay::backend::allocator::dmabuf::Dmabuf> {
        // Through the backend's own renderer, which is where the allocator
        // lives. Only reachable from outside the render path — see
        // `allocate_cast_targets`.
        let Some(mut udev) = self.udev.take() else {
            return Vec::new();
        };
        // DMA-BUF targets come from the Vulkan renderer's allocator; GLES has
        // no `Offscreen<Dmabuf>`, so a screen share under it takes the
        // shared-memory path instead of handing buffers over.
        let targets = match &mut udev.primary_mut().renderer {
            crate::udev::Gpu::Vulkan(renderer) => Self::allocate_cast_targets(renderer, size),
            _ => Vec::new(),
        };
        self.udev = Some(udev);
        targets
    }

    /// Allocate against a renderer that is already in hand.
    ///
    /// Taking the renderer rather than reaching for `self.udev`, because the
    /// render path has already moved it out of the state — it has to, to lend
    /// it out while calling back into the compositor. Reaching for it there
    /// found nothing and returned no buffers, and a stream with no buffers to
    /// offer advertises no DMA-BUF format at all: every renegotiation quietly
    /// dropped the share onto the shared-memory path, which is the readback
    /// per frame this was written to avoid. Nothing said so, because "could
    /// not allocate" and "this backend has no allocator" looked the same.
    pub(crate) fn allocate_cast_targets<R>(
        renderer: &mut R,
        size: smithay::utils::Size<i32, smithay::utils::Physical>,
    ) -> Vec<smithay::backend::allocator::dmabuf::Dmabuf>
    where
        R: Offscreen<smithay::backend::allocator::dmabuf::Dmabuf>,
    {
        let buffer_size: smithay::utils::Size<i32, smithay::utils::Buffer> =
            (size.w.max(1), size.h.max(1)).into();

        let mut targets = Vec::with_capacity(crate::screencast::stream::BUFFERS);
        for _ in 0..crate::screencast::stream::BUFFERS {
            // The same format the readback path used, which is what the stream
            // describes to the consumer: four bytes a pixel, no alpha, because
            // a screen is opaque and a consumer that reads the fourth byte as
            // alpha shows a transparent picture.
            match renderer.create_buffer(smithay::backend::allocator::Fourcc::Xrgb8888, buffer_size)
            {
                Ok(target) => targets.push(target),
                Err(e) => {
                    tracing::warn!("could not allocate a screencast buffer: {e}");
                    return Vec::new();
                }
            }
        }
        targets
    }

    /// Stop sharing whatever a session was showing.
    pub fn stop_cast(&mut self, node: u32) {
        let before = self.casts.len();
        self.casts.retain(|cast| cast.stream.node_id != node);
        if self.casts.len() != before {
            tracing::info!("stopped sharing on pipewire node {node}");
        }
        if self.casts.is_empty() {
            // Nothing is being shared, so the connection is not worth holding.
            self.pipewire = None;
        }
    }

    /// Hand this output's frame to anything sharing it.
    pub fn feed_casts<R, B>(&mut self, output: &Output, renderer: &mut R)
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
        if self.casts.is_empty() {
            return;
        }

        // What a share is worth asking the renderer for.
        //
        // Compositing and reading back a screen is a full frame off the GPU —
        // fifteen megabytes at 1440p — and doing it at the compositor's own
        // rate made the desktop lag while a share was open. Thirty a second
        // is what a screen share is watched at.
        const RATE: std::time::Duration = std::time::Duration::from_millis(33);
        if !self.casts.iter().any(|cast| cast.stream.wants_frame(RATE)) {
            return;
        }

        // The streams that take a buffer the GPU drew into, first and one at a
        // time. Each is composited straight into the memory the consumer will
        // read, so there is nothing to share between them and nothing to copy.
        self.draw_into_casts(output, renderer);

        // What each share names right now. Resolved once and reused, because a
        // following source is answered from focus and the answer must not
        // change between deciding to composite and deciding who receives it —
        // that is a frame handed to the wrong stream at the wrong size.
        let targets = self.cast_targets_now();

        // Then the ones that need pixels in shared memory. One composite and
        // one readback serves every client watching this output.
        let watching_output = self.casts.iter().zip(targets.iter()).any(|(cast, target)| {
            cast.stream.wants_frame(RATE)
                && !cast.stream.uses_dmabuf()
                && matches!(target, Some(crate::screencast::Target::Output(o)) if o == output)
        });
        if watching_output {
            if let Some(size) = output
                .current_mode()
                .map(|mode| output.current_transform().transform_size(mode.size))
            {
                let region = smithay::utils::Rectangle::from_size((size.w, size.h).into());
                // The cursor is drawn in: this is a picture of a screen rather
                // than a screenshot of one, and a share without a pointer is
                // hard to follow.
                match self.read_output_pixels::<R, B>(output, region, true, renderer) {
                    Ok(pixels) => self.push_to_casts(
                        &targets,
                        |target| {
                            matches!(target, crate::screencast::Target::Output(o) if o == output)
                        },
                        &pixels,
                        size,
                    ),
                    Err(e) => tracing::warn!("could not read a frame for a screencast: {e}"),
                }
            }
        }

        // Then the whole desk, if anything is watching it — once per frame
        // rather than once per monitor, on whichever output does the work.
        let watching_desk = self.casts.iter().zip(targets.iter()).any(|(cast, target)| {
            cast.stream.wants_frame(RATE)
                && !cast.stream.uses_dmabuf()
                && matches!(target, Some(crate::screencast::Target::AllOutputs))
        });
        if watching_desk && self.desk_capture_output().as_ref() == Some(output) {
            match self.read_desk_pixels::<R, B>(renderer) {
                Ok((pixels, size)) => self.push_to_casts(
                    &targets,
                    |target| matches!(target, crate::screencast::Target::AllOutputs),
                    &pixels,
                    size,
                ),
                Err(e) => tracing::warn!("could not read the desk for a screencast: {e}"),
            }
        }

        // Then windows, one composite each. A window is shared as itself
        // rather than as the part of the screen it covers: whatever is on top
        // of it belongs to the desktop, not to the thing being shared.
        //
        // Each window once, however many streams are watching it: a share
        // that follows the focused window and a share of that same window by
        // name both resolve here, and compositing it twice would cost a whole
        // window per extra viewer for an identical picture.
        let mut windows: Vec<u32> = self
            .casts
            .iter()
            .zip(targets.iter())
            .filter(|(cast, _)| cast.stream.wants_frame(RATE) && !cast.stream.uses_dmabuf())
            .filter_map(|(_, target)| match target {
                Some(crate::screencast::Target::Window(id)) => Some(*id),
                _ => None,
            })
            .collect();
        windows.sort_unstable();
        windows.dedup();
        for id in windows {
            // Only from the one output that serves it, so a window straddling
            // two screens is composited once per period and not once for each.
            let on_this_output = self
                .views
                .get(id)
                .and_then(|view| self.space.element_geometry(&view.window))
                .is_some_and(|geometry| self.window_cast_served_by(geometry, output));
            if !on_this_output {
                continue;
            }
            match self.read_window_pixels::<R, B>(id, renderer) {
                Ok((pixels, size)) => self.push_to_casts(
                    &targets,
                    |target| matches!(target, crate::screencast::Target::Window(other) if *other == id),
                    &pixels,
                    size,
                ),
                Err(e) => tracing::warn!("could not read a window for a screencast: {e}"),
            }
        }

        // Keep drawing while anything is watching.
        //
        // Rendering is driven by damage, and a desktop nobody is touching
        // produces none — so the compositor drew one frame, handed it over,
        // and stopped. A share is a stream: the viewer needs a frame whether
        // or not this end has changed, and one that stops arriving reads as a
        // frozen screen rather than a still one.
        self.needs_render = true;
    }

    /// What a source names right now.
    ///
    /// `None` is "nothing to point at just now" rather than an error: a share
    /// following the focused window has nothing to show while focus is
    /// nowhere, and the honest thing to do is leave the last frame up until
    /// there is a window again. Tearing the share down would mean a click on
    /// the desktop ended the meeting.
    fn resolve_cast(
        &self,
        source: &crate::screencast::Source,
    ) -> Option<crate::screencast::Target> {
        use crate::screencast::{Source, Target};
        match source {
            Source::Output(output) => Some(Target::Output(output.clone())),
            Source::Window(id) => self
                .views
                .get(*id)
                .filter(|view| view.mapped)
                .map(|view| Target::Window(view.id)),
            Source::AllOutputs => self.space.outputs().next().map(|_| Target::AllOutputs),
            Source::FollowOutput => self
                .active_output
                .as_ref()
                .and_then(|name| self.output_by_name(name))
                .or_else(|| self.space.outputs().next().cloned())
                .map(Target::Output),
            Source::FollowWindow => self
                .views
                .get(self.focused)
                .filter(|view| view.mapped)
                .map(|view| Target::Window(view.id)),
        }
    }

    /// Where on the desk a point inside one of this session's streams lands.
    ///
    /// The only coordinate space a remote application has. It is looking at a
    /// picture — of a monitor, of a window, of the whole desk — and it clicks
    /// on what it sees, so what reaches the portal is a position inside that
    /// picture and a node number saying which picture. Turning that back into
    /// a place on the desk is this end's job, and nothing else can do it: the
    /// application has never been told where the window is, and it must not be.
    ///
    /// Mapped proportionally rather than by adding an origin and dividing by a
    /// scale. The two are the same for the ordinary case and only the
    /// proportional one survives the others: a rotated monitor streams at its
    /// transformed size, a HiDPI one streams at more pixels than it occupies
    /// in the layout, and a share of the whole desk streams at the largest
    /// scale of any monitor on it (see `all_outputs_layout`). One ratio covers
    /// all three, and the alternative is three special cases that each go
    /// wrong on somebody's desk.
    ///
    /// `None` when the node names no stream this compositor handed out, or
    /// when what it was following has gone — a share of the focused window
    /// with focus nowhere. Dropping the event is the right answer to both:
    /// guessing at a position would click on whatever happened to be under it.
    pub fn remote_point(&self, node: u32, x: f64, y: f64) -> Option<Point<f64, Logical>> {
        let cast = self.casts.iter().find(|cast| cast.stream.node_id == node)?;
        let target = self.resolve_cast(&cast.source)?;
        let rect = self.target_rect(&target)?;
        let size = self.target_size(&target)?;
        if size.w <= 0 || size.h <= 0 {
            return None;
        }
        let inside: Point<f64, Logical> = (
            x * rect.size.w as f64 / size.w as f64,
            y * rect.size.h as f64 / size.h as f64,
        )
            .into();
        Some(rect.loc.to_f64() + inside)
    }

    /// Where a captured thing sits in the layout, in the layout's own units.
    ///
    /// The companion to `target_size`, which answers the same question in the
    /// pixels the stream carries. Both are needed together and only together:
    /// one says how big the picture is and the other says what part of the
    /// desk it is a picture of.
    fn target_rect(&self, target: &crate::screencast::Target) -> Option<Rectangle<i32, Logical>> {
        match target {
            crate::screencast::Target::Output(output) => self.space.output_geometry(output),
            crate::screencast::Target::Window(id) => {
                let view = self.views.get(*id)?;
                self.space.element_geometry(&view.window)
            }
            crate::screencast::Target::AllOutputs => {
                self.all_outputs_layout().map(|(union, _)| union)
            }
        }
    }

    /// What to call a stream, which is what a consumer shows in its own list.
    ///
    /// From the source rather than from what it currently resolves to: the name
    /// is fixed at negotiation and a following share that renamed itself every
    /// time focus moved would be a recorder whose file name is whatever window
    /// was in front when it stopped.
    fn cast_name(
        &self,
        source: &crate::screencast::Source,
        target: &crate::screencast::Target,
    ) -> String {
        use crate::screencast::{Source, Target};
        match (source, target) {
            (Source::AllOutputs, _) => "all monitors".to_owned(),
            (Source::FollowOutput, _) => "the active monitor".to_owned(),
            (Source::FollowWindow, _) => "the focused window".to_owned(),
            (_, Target::Output(output)) => output.name(),
            (_, Target::Window(id)) => self
                .views
                .get(*id)
                .map(|view| view.title())
                .unwrap_or_else(|| "a window".to_owned()),
            (_, Target::AllOutputs) => "all monitors".to_owned(),
        }
    }

    /// The size a source is now, whatever it was when the share started.
    fn cast_size(
        &self,
        source: &crate::screencast::Source,
    ) -> Option<smithay::utils::Size<i32, smithay::utils::Physical>> {
        self.target_size(&self.resolve_cast(source)?)
    }

    /// How big a picture of this would be.
    fn target_size(
        &self,
        target: &crate::screencast::Target,
    ) -> Option<smithay::utils::Size<i32, smithay::utils::Physical>> {
        match target {
            crate::screencast::Target::Output(output) => output
                .current_mode()
                .map(|mode| output.current_transform().transform_size(mode.size)),
            crate::screencast::Target::Window(id) => {
                let view = self.views.get(*id)?;
                let geometry = self.space.element_geometry(&view.window)?;
                Some((geometry.size.w.max(1), geometry.size.h.max(1)).into())
            }
            crate::screencast::Target::AllOutputs => self.desk_size(),
        }
    }

    /// How big a picture of the whole desk is.
    ///
    /// At least one pixel each way: an empty layout would otherwise negotiate
    /// a zero-sized stream, which PipeWire accepts and no consumer can read.
    fn desk_size(&self) -> Option<smithay::utils::Size<i32, smithay::utils::Physical>> {
        let (union, scale) = self.all_outputs_layout()?;
        let size: smithay::utils::Size<i32, smithay::utils::Physical> =
            union.size.to_f64().to_physical(scale).to_i32_round();
        Some((size.w.max(1), size.h.max(1)).into())
    }

    /// The rectangle every monitor sits inside, and the scale to draw it at.
    ///
    /// The largest scale of any of them, not the smallest and not one: the
    /// point of sharing the whole desk is that somebody watching can read what
    /// is on it, and a two-monitor desk where one screen is HiDPI would
    /// otherwise be captured with that screen halved. Oversampling the coarser
    /// monitor costs pixels; undersampling the finer one costs the text.
    fn all_outputs_layout(&self) -> Option<(Rectangle<i32, Logical>, f64)> {
        let mut union: Option<Rectangle<i32, Logical>> = None;
        let mut scale: f64 = 1.0;
        for output in self.space.outputs() {
            let Some(geometry) = self.space.output_geometry(output) else {
                continue;
            };
            scale = scale.max(output.current_scale().fractional_scale());
            union = Some(match union {
                Some(union) => union.merge(geometry),
                None => geometry,
            });
        }
        union.map(|union| (union, scale))
    }

    /// Which output does the work for a share of the whole desk.
    ///
    /// Every output's frame calls into the capture path, and a picture of the
    /// desk is the same picture whichever of them asked for it — so it is
    /// composited on one of their frames and skipped on the rest. Without this
    /// a three-monitor desk composited the whole layout three times a frame.
    ///
    /// The first in the space's order, which is stable for as long as the set
    /// of monitors is: any one of them would do, and one that moved would mean
    /// a frame missed or drawn twice each time it changed.
    fn desk_capture_output(&self) -> Option<Output> {
        self.space.outputs().next().cloned()
    }

    /// Whether `output` is the one that serves a window's casts.
    ///
    /// A window straddling two screens is on both, and every output's frame
    /// walks the casts — so an overlap test alone composited it once per
    /// screen it touched, twice the work for one picture. The rule here is
    /// one serving output per window, the one it overlaps most, with the
    /// space's order breaking ties; the same answer on both paths that ask
    /// (the shared-memory loop and the DMA-BUF arm), which is what makes the
    /// two agree that a window is drawn once per period.
    fn window_cast_served_by(&self, geometry: Rectangle<i32, Logical>, output: &Output) -> bool {
        let names = self
            .space
            .outputs()
            .map(|other| (other.name(), self.space.output_geometry(other)));
        serving_cast_output(names, geometry).is_some_and(|name| name == output.name())
    }

    /// Agree a new format for anything whose source has resized.
    ///
    /// Called from the backend before it feeds them, because only the backend
    /// can allocate: `allocate` is handed the new size and answers with the
    /// buffers the stream will hand out, or with none — which is right for a
    /// nested session, whose streams are shared memory anyway and whose memory
    /// is allocated when PipeWire asks rather than up front.
    ///
    /// A closure rather than the renderer, because the renderer is generic in
    /// the render pass and only one of the two can allocate at all. See
    /// `Captures::cast_targets`.
    pub fn resize_casts<F>(&mut self, mut allocate: F)
    where
        F: FnMut(
            smithay::utils::Size<i32, smithay::utils::Physical>,
        ) -> Vec<smithay::backend::allocator::dmabuf::Dmabuf>,
    {
        let resized: Vec<(usize, smithay::utils::Size<i32, smithay::utils::Physical>)> = self
            .casts
            .iter()
            .enumerate()
            .filter_map(|(at, cast)| {
                let size = self.cast_size(&cast.source)?;
                cast.stream.needs_renegotiation(size).then_some((at, size))
            })
            .collect();
        if resized.is_empty() {
            return;
        }

        for (at, size) in resized {
            // Buffers of the new size, before the offer goes out: the consumer
            // may take the format at once and ask for them on its own thread.
            let targets = allocate(size);
            let mut casts = std::mem::take(&mut self.casts);
            if let (Some(cast), Some(pipewire)) = (casts.get_mut(at), self.pipewire.as_ref()) {
                if let Err(e) = cast
                    .stream
                    .renegotiate(size, targets, &pipewire.thread_loop)
                {
                    tracing::warn!("could not resize a screencast: {e}");
                }
            }
            self.casts = casts;
        }
    }

    /// Composite a frame straight into the buffer each waiting stream will
    /// hand to its consumer.
    ///
    /// The point of the whole DMA-BUF path: the shared-memory one reads a
    /// screen back off the GPU and writes it out again — fifteen megabytes a
    /// frame at 1440p, thirty times a second — and this one draws where the
    /// consumer is already looking.
    fn draw_into_casts<R>(&mut self, output: &Output, renderer: &mut R)
    where
        R: Renderer
            + Bind<smithay::backend::allocator::dmabuf::Dmabuf>
            + smithay::backend::renderer::ImportAll
            + smithay::backend::renderer::ImportMem
            + smithay::backend::renderer::ImportDma,
        <R as smithay::backend::renderer::RendererSuper>::TextureId: Clone + Send + Sync + 'static,
        <R as smithay::backend::renderer::RendererSuper>::Error: Send + Sync + 'static,
    {
        const RATE: std::time::Duration = std::time::Duration::from_millis(33);

        // What each share names right now, before the casts are taken out of
        // the state: resolving a following source needs the state, and this
        // borrows it immutably while `self.casts` is still there to line up
        // with.
        let targets = self.cast_targets_now();
        // Whether a share of the whole desk is this output's job, worked out
        // for the same reason.
        let desk_is_ours = self.desk_capture_output().as_ref() == Some(output);

        // Both taken out for the duration: compositing needs the whole state,
        // and the stream being drawn into is part of it.
        let mut casts = std::mem::take(&mut self.casts);
        let pipewire = self.pipewire.take();
        if let Some(pipewire) = pipewire.as_ref() {
            for (cast, target) in casts.iter_mut().zip(targets.iter()) {
                if !cast.stream.uses_dmabuf() || !cast.stream.wants_frame(RATE) {
                    continue;
                }
                match target {
                    Some(crate::screencast::Target::Output(shared)) if shared == output => {
                        let shared = shared.clone();
                        let size = match shared.current_mode() {
                            Some(mode) => shared.current_transform().transform_size(mode.size),
                            None => continue,
                        };
                        cast.stream
                            .with_target(size, &pipewire.thread_loop, |target| {
                                // The cursor is drawn in: this is a picture of
                                // a screen rather than a screenshot of one, and
                                // a share without a pointer is hard to follow.
                                self.render_output_into(&shared, target.clone(), true, renderer)
                            });
                    }
                    Some(crate::screencast::Target::Window(id)) => {
                        let id = *id;
                        // Only from the output that serves it — the one it
                        // overlaps most — so a window straddling two screens is
                        // composited once for the period, not once for each.
                        let geometry = self
                            .views
                            .get(id)
                            .and_then(|view| self.space.element_geometry(&view.window));
                        let on_this_output = geometry
                            .is_some_and(|geometry| self.window_cast_served_by(geometry, output));
                        let Some(geometry) = geometry.filter(|_| on_this_output) else {
                            continue;
                        };
                        let size = (geometry.size.w.max(1), geometry.size.h.max(1)).into();
                        cast.stream
                            .with_target(size, &pipewire.thread_loop, |target| {
                                self.render_window_into(id, target.clone(), renderer)
                            });
                    }
                    // One monitor's frame does the desk, and the rest skip it:
                    // the picture is the same whichever of them asked.
                    Some(crate::screencast::Target::AllOutputs) if desk_is_ours => {
                        let Some(size) = self.desk_size() else {
                            continue;
                        };
                        cast.stream
                            .with_target(size, &pipewire.thread_loop, |target| {
                                self.render_desk_into(target.clone(), renderer)
                            });
                    }
                    _ => {}
                }
            }
        }
        self.pipewire = pipewire;
        self.casts = casts;
    }

    /// What every running share names right now, in `self.casts` order.
    ///
    /// Kept alongside the casts rather than inside them: a following source is
    /// answered from the compositor's state, and storing the answer would mean
    /// deciding when to refresh it. This way there is nothing to keep in step.
    fn cast_targets_now(&self) -> Vec<Option<crate::screencast::Target>> {
        self.casts
            .iter()
            .map(|cast| self.resolve_cast(&cast.source))
            .collect()
    }

    /// Hand a frame to every cast a predicate matches.
    ///
    /// Matched on what each share resolves to rather than on what it asked
    /// for, so a stream following the focused window is fed by the same
    /// composite that feeds a stream naming that window outright. `targets`
    /// runs alongside `self.casts`; a share that resolves to nothing is fed
    /// nothing.
    fn push_to_casts(
        &mut self,
        targets: &[Option<crate::screencast::Target>],
        matches: impl Fn(&crate::screencast::Target) -> bool,
        pixels: &[u8],
        size: smithay::utils::Size<i32, smithay::utils::Physical>,
    ) {
        let mut casts = std::mem::take(&mut self.casts);
        if let Some(pipewire) = self.pipewire.as_ref() {
            for (cast, target) in casts.iter_mut().zip(targets.iter()) {
                let feed = !cast.stream.uses_dmabuf() && target.as_ref().is_some_and(&matches);
                if feed {
                    cast.stream.push(pixels, size, &pipewire.thread_loop);
                }
            }
        }
        self.casts = casts;
    }

    /// Whether a view is showing on this output at all.
    ///
    /// Overlap, not containment: a window straddling two screens is on both,
    /// and the caller picks one. False for a view that has gone — which is how
    /// a capture of a closed window stops being anybody's to serve.
    pub fn window_is_on(&self, id: u32, output: &Output) -> bool {
        self.views
            .get(id)
            .and_then(|view| self.space.element_geometry(&view.window))
            .zip(self.space.output_geometry(output))
            .map(|(window, screen)| screen.overlaps(window))
            .unwrap_or(false)
    }

    /// Composite one window and copy it into a client's shared memory.
    ///
    /// The shm half of `render_window_into`, and the same relationship
    /// `copy_output_into` has to `render_output_into`: a client that could not
    /// allocate a DMA-BUF still gets its picture, at the cost of reading every
    /// pixel back.
    fn copy_window_into<R, B>(
        &mut self,
        id: u32,
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
        let (pixels, size) = self.read_window_pixels::<R, B>(id, renderer)?;

        blit_shm(buffer, &pixels, size, "the window")?;

        Ok(())
    }

    /// Composite one window straight into a buffer a consumer will read.
    ///
    /// The same picture `read_window_pixels` produces, drawn where it is going
    /// rather than read back and copied.
    fn render_window_into<R>(
        &mut self,
        id: u32,
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
        let (elements, size) = self.window_elements(id, renderer)?;

        let mut framebuffer = renderer
            .bind(&mut target)
            .map_err(|e| format!("binding a window capture target: {e}"))?;
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
            .map_err(|e| format!("compositing a window: {e:?}"))?;

        // Waited for, because nothing else will. Rendering returns once the
        // work is submitted, and a consumer handed the buffer the GPU is still
        // writing into reads whatever was there before.
        result
            .sync
            .wait()
            .map_err(|e| format!("waiting for a window capture to finish: {e}"))
    }

    /// One window's own surface tree, drawn at its own origin.
    ///
    /// Its own tree rather than the part of the screen it occupies: what is on
    /// top of a window belongs to the desktop, and a client that asked to share
    /// a window did not ask to share whatever is covering it. Drawn at the
    /// window's origin so the shadow a client draws outside its geometry falls
    /// off the edge rather than shifting the picture.
    ///
    /// Nothing at all while the session is locked, which every caller composites
    /// as a black frame of the right size. A screen is blanked for a lock by
    /// `frame_for` — see `Frame::locked_blank` — and a window is not drawn
    /// through that at all: it is its own surface tree, composited here, so a
    /// share of a window went on streaming what was in it across the lock
    /// screen. A share that stops rather than freezes: the last frame before
    /// the lock is as much of the desktop as the next one would be.
    fn window_elements<R>(&mut self, id: u32, renderer: &mut R) -> Result<WindowElements<R>, String>
    where
        R: Renderer + smithay::backend::renderer::ImportAll,
        <R as smithay::backend::renderer::RendererSuper>::TextureId: Clone + Send + Sync + 'static,
    {
        use smithay::backend::renderer::element::surface::render_elements_from_surface_tree;
        use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
        use smithay::backend::renderer::element::Kind;
        use smithay::wayland::seat::WaylandFocus as _;

        let view = self
            .views
            .get(id)
            .ok_or_else(|| "no such window".to_owned())?;
        let window = view.window.clone();
        let geometry = window.geometry();
        let size: smithay::utils::Size<i32, smithay::utils::Physical> =
            (geometry.size.w.max(1), geometry.size.h.max(1)).into();
        let surface = window
            .wl_surface()
            .ok_or_else(|| "that window has no surface".to_owned())?
            .into_owned();
        if self.locked || !view.capture_allowed {
            return Ok((Vec::new(), size));
        }

        let elements = render_elements_from_surface_tree::<_, WaylandSurfaceRenderElement<R>>(
            renderer,
            &surface,
            (-geometry.loc.x, -geometry.loc.y),
            1.0,
            1.0,
            Kind::Unspecified,
        );
        Ok((elements, size))
    }

    /// Composite one window on its own, and read it back.
    ///
    /// Its own surface tree rather than the part of the screen it occupies:
    /// what is on top of a window belongs to the desktop, and a client that
    /// asked to share a window did not ask to share whatever is covering it.
    fn read_window_pixels<R, B>(
        &mut self,
        id: u32,
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
        let (elements, size) = self.window_elements(id, renderer)?;

        let format = smithay::backend::allocator::Fourcc::Xrgb8888;
        let buffer_size: smithay::utils::Size<i32, smithay::utils::Buffer> =
            (size.w, size.h).into();
        let mut target: B = self.take_capture_target(renderer, format, buffer_size)?;

        let mapping = {
            let mut framebuffer = renderer
                .bind(&mut target)
                .map_err(|e| format!("binding a window capture target: {e}"))?;
            // A window, not an output: its own size, upright. A window is not
            // rotated by the screen it happens to be on — what a client asked
            // to capture is the window.
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
                .map_err(|e| format!("compositing a window: {e:?}"))?;
            renderer
                .copy_framebuffer(
                    &framebuffer,
                    smithay::utils::Rectangle::from_size(buffer_size),
                    format,
                )
                .map_err(|e| format!("reading a window back: {e}"))?
        };
        let pixels = renderer
            .map_texture(&mapping)
            .map_err(|e| format!("mapping a window capture: {e}"))?
            .to_vec();
        self.keep_capture_target(format, buffer_size, target);
        Ok((pixels, size))
    }

    /// Carry out what the portal asked for.
    pub fn handle_screencast(&mut self, message: crate::screencast::portal::Message) {
        use crate::screencast::portal::Message;

        match message {
            Message::Start {
                types,
                restore,
                reply,
            } => self.open_screencast_picker(types, restore, reply),
            Message::StartRemote {
                devices,
                types,
                reply,
            } => self.open_remote_picker(devices, types, reply),
            Message::Inject(injection) => self.inject_remote(injection),
            // The reading half of the socket ConnectToEIS answered with. The
            // bus thread made the pair and checked the grant; this end builds
            // the EI context, because the context is read by a calloop source
            // and calloop is here. See [`crate::libei`].
            Message::ConnectEis {
                session,
                stream,
                devices,
            } => self.connect_eis(session, stream, devices),
            Message::RevokeEis { session } => self.revoke_eis(&session),
            Message::Close { node } => self.stop_cast(node),
        }
    }

    /// Carry out a screenshot portal request.
    pub fn handle_screenshot(&mut self, message: crate::screenshot::Message) {
        use crate::screenshot::Message;

        match message {
            Message::Capture {
                interactive: _,
                modal: _,
                reply,
            } => {
                let output = self
                    .active_output
                    .as_deref()
                    .and_then(|name| self.space.outputs().find(|o| o.name() == name))
                    .cloned()
                    .or_else(|| self.space.outputs().next().cloned());
                self.pending_screenshots
                    .push(crate::screenshot::PendingScreenshot {
                        output,
                        window_id: None,
                        reply,
                    });
            }
        }
    }

    /// How long a chooser stays up before it gives up on being answered.
    ///
    /// The application is waiting on this: its own dialogue says the share is
    /// starting for as long as the chooser is open, so a user who walked away
    /// leaves it there. Long enough to read the list and think about it.
    const PICK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

    /// Ask the user what to share.
    fn open_screencast_picker(
        &mut self,
        types: u32,
        restore: Option<crate::screencast::Remembered>,
        reply: async_channel::Sender<Result<crate::screencast::portal::Started, String>>,
    ) {
        // One at a time. Two choosers on screen with one keyboard between them
        // is a race the user cannot see, let alone win.
        if self.picker.is_some() {
            let _ = reply.try_send(Err("something else is already being chosen".to_owned()));
            return;
        }

        // The same thing as last time, if the application asked for that and
        // the thing is still there. Before the chooser rather than as a row in
        // it: the point of a remembered share is that a recorder set up in
        // March still records the right screen in June without anybody at the
        // keyboard, and a chooser that has to be answered is exactly what the
        // application asked to avoid.
        if let Some(remembered) = restore {
            match self.restore_source(&remembered, types) {
                Some(source) => {
                    // The remembered form rather than the source: an `Output`
                    // prints its every mode and instance, and a line nobody
                    // can read in a log is a line that is not there.
                    tracing::info!("sharing {remembered:?} again, as the application asked");
                    self.begin_cast(source, 0, crate::screencast::Reply::Cast(reply));
                    return;
                }
                // Not a failure. The monitor is unplugged or the window is
                // closed, and the honest answer is the chooser — sharing some
                // other screen because it is the one left would hand over a
                // desk nobody agreed to.
                None => tracing::info!(
                    "the application asked to share {remembered:?} again, \
                     which is not here, so asking"
                ),
            }
        }

        let sources = self.screencast_sources(types);
        if sources.is_empty() {
            let _ = reply.try_send(Err("there is nothing to share".to_owned()));
            return;
        }

        // No trusted UI means no consent. Sharing a fallback source here used
        // to expose the desktop after a shell crash or during startup.
        if !self.shell_can_draw()
            && std::env::var("VIEWPORT_UNSAFE_NO_CONSENT").as_deref() != Ok("1")
        {
            tracing::warn!("refusing screen sharing because no consent UI is available");
            let _ = reply.try_send(Err("no consent UI is available".to_owned()));
            return;
        }

        if !self.shell_can_draw() {
            let source = sources.into_iter().next().expect("checked above");
            tracing::warn!(
                "VIEWPORT_UNSAFE_NO_CONSENT=1: sharing {} without asking",
                source.describe()
            );
            self.begin_cast(source, 0, crate::screencast::Reply::Cast(reply));
            return;
        }

        self.raise_picker(
            sources,
            0,
            Vec::new(),
            String::new(),
            crate::screencast::Reply::Cast(reply),
        );
    }

    /// Put a chooser on screen and hand the keyboard to it.
    ///
    /// Everything the two kinds of chooser have in common, which is all of it
    /// except what is being asked: minting an id so a late answer cannot land
    /// on a later question, taking the keys, telling the shell, and arming the
    /// clock that answers for a user who walked away. Shared rather than
    /// copied because the remote-desktop chooser is the same dialogue with a
    /// different sentence at the top, and a second copy of this would be a
    /// second place for the focus restore or the timeout to be forgotten.
    fn raise_picker(
        &mut self,
        sources: Vec<crate::screencast::Source>,
        devices: u32,
        shortcuts: Vec<crate::shortcuts::Granted>,
        app: String,
        reply: crate::screencast::Reply,
    ) {
        let id = self.next_pick;
        self.next_pick = self.next_pick.wrapping_add(1).max(1);
        self.picker = Some(crate::screencast::Picker {
            id,
            sources,
            selected: 0,
            restore: self.focused,
            devices,
            shortcuts,
            app,
            reply,
        });

        // The keys have to come here rather than to whatever was focused: the
        // chooser is driven from the compositor, and a keystroke meant for it
        // that reached a terminal instead would be typed into it.
        if let Some(keyboard) = self.seat.get_keyboard() {
            let serial = smithay::utils::SERIAL_COUNTER.next_serial();
            keyboard.set_focus(
                self,
                Option::<crate::keyboard_focus::KeyboardFocus>::None,
                serial,
            );
        }
        self.notify_picker();

        // Answered either way, in the end. An application left waiting on a
        // chooser nobody is looking at shows a share that is forever about to
        // start.
        let _ = self.loop_handle.insert_source(
            smithay::reexports::calloop::timer::Timer::from_duration(Self::PICK_TIMEOUT),
            move |_, _, state| {
                if state.picker.as_ref().is_some_and(|picker| picker.id == id) {
                    tracing::info!("nobody answered the chooser");
                    state.cancel_screencast_pick();
                }
                smithay::reexports::calloop::timer::TimeoutAction::Drop
            },
        );
    }

    /// Answer a global-shortcuts request.
    ///
    /// Three ways it can go, and only the middle one puts anything on screen.
    /// Nothing this keymap can match is refused without asking — see
    /// `Granted::from_request`, and note that a partial list is still put to
    /// the person, because what they are agreeing to is what the application
    /// will actually end up holding. A list this application has already
    /// agreed to is granted on the spot; anything else is a dialogue.
    pub fn handle_shortcuts(&mut self, message: crate::shortcuts::Message) {
        match message {
            crate::shortcuts::Message::Bind {
                app_id,
                session,
                shortcuts,
                reply,
            } => {
                let wanted: Vec<crate::shortcuts::Granted> = shortcuts
                    .iter()
                    .filter_map(|request| {
                        let granted = crate::shortcuts::Granted::from_request(request);
                        if granted.is_none() {
                            // Said out loud: from the application's side this
                            // is a shortcut that never fires, and the reason
                            // is a trigger this desktop could not read.
                            tracing::info!(
                                "shortcuts: {app_id:?} asked for {:?} on {:?}, which is not a chord here",
                                request.id,
                                request.trigger
                            );
                        }
                        granted
                    })
                    .collect();

                if wanted.is_empty() {
                    let _ = reply.try_send(Err("no shortcut here could be matched".to_owned()));
                    return;
                }

                // Already agreed to, so not asked again. The record is by
                // application and by chord; see `shortcuts::Store`.
                if self.shortcuts_store.covers(&app_id, &wanted) {
                    tracing::info!(
                        "shortcuts: {app_id:?} keeps {} it was already given",
                        crate::shortcuts::count(wanted.len())
                    );
                    self.grant_shortcuts(&session, &app_id, &wanted);
                    let _ = reply.try_send(Ok(wanted));
                    return;
                }

                if self.picker.is_some() {
                    let _ =
                        reply.try_send(Err("something else is already being chosen".to_owned()));
                    return;
                }
                // The same refusal the remote-desktop path makes, for the same
                // reason: a machine that hands out keys because its own user
                // interface is broken has turned a shell bug into a way in.
                if !self.shell_can_draw() {
                    tracing::warn!(
                        "no desktop page is drawing, so refusing to give an application a global shortcut"
                    );
                    let _ = reply.try_send(Err("there is nobody to ask".to_owned()));
                    return;
                }

                tracing::info!(
                    "{app_id:?} is asking for {}: {}",
                    crate::shortcuts::count(wanted.len()),
                    wanted
                        .iter()
                        .map(|shortcut| shortcut.chord.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                self.pending_shortcuts = Some((session, app_id.clone()));
                self.raise_picker(
                    Vec::new(),
                    0,
                    wanted,
                    app_id,
                    crate::screencast::Reply::Shortcuts(reply),
                );
            }
            crate::shortcuts::Message::List { session, reply } => {
                let held: Vec<crate::shortcuts::Granted> = self
                    .shortcuts
                    .iter()
                    .filter(|grant| grant.session == session)
                    .map(|grant| grant.shortcut.clone())
                    .collect();
                let _ = reply.try_send(Ok(held));
            }
            crate::shortcuts::Message::Close { session } => {
                let before = self.shortcuts.len();
                self.shortcuts.retain(|grant| grant.session != session);
                // And anything of this session's that was held down when it
                // went. Nothing will ever release it otherwise, and the key it
                // is on would go on being swallowed from every window.
                self.shortcuts_held
                    .retain(|(_, fired)| fired.session != session);
                if self.shortcuts.len() != before {
                    tracing::debug!("shortcuts: session {session} closed");
                }
            }
        }
    }

    /// Write a granted list into the table the key path reads.
    ///
    /// A session that binds twice replaces what it had rather than
    /// accumulating: `BindShortcuts` describes the shortcuts the application
    /// wants *now*, and a shortcut it has stopped asking for should stop being
    /// taken from the window under it.
    pub fn grant_shortcuts(
        &mut self,
        session: &zvariant::OwnedObjectPath,
        app_id: &str,
        shortcuts: &[crate::shortcuts::Granted],
    ) {
        self.shortcuts.retain(|grant| &grant.session != session);
        for shortcut in shortcuts {
            self.shortcuts.push(crate::shortcuts::Grant {
                session: session.clone(),
                app_id: app_id.to_owned(),
                shortcut: shortcut.clone(),
            });
        }
    }

    /// Which granted shortcut a key press is, if it is one.
    ///
    /// Asked only after the compositor's own chords and the configured
    /// bindings have both declined it, so a shortcut can never take a key the
    /// desktop itself is using — an application that asks for `Mod4+Return`
    /// gets a grant that never fires rather than a terminal that stops
    /// opening.
    pub fn shortcut_for(
        &self,
        modifiers: &smithay::input::keyboard::ModifiersState,
        keysym: u32,
    ) -> Option<crate::shortcuts::Fired> {
        let wanted = crate::binding::Modifiers::from_state(modifiers);
        self.shortcuts
            .iter()
            .find(|grant| grant.shortcut.keysym == keysym && grant.shortcut.modifiers == wanted)
            .map(|grant| crate::shortcuts::Fired {
                session: grant.session.clone(),
                id: grant.shortcut.id.clone(),
            })
    }

    /// Announce everything the key filter noticed.
    ///
    /// Drained after the filter rather than inside it: the filter runs under
    /// the keyboard's own borrow, and a D-Bus write from there happens with
    /// the input path's lock held.
    pub fn flush_shortcuts(&mut self) {
        if self.shortcuts_to_announce.is_empty() {
            return;
        }
        let timestamp = self.start_time.elapsed().as_micros() as u64;
        for (activated, fired) in std::mem::take(&mut self.shortcuts_to_announce) {
            self.shortcut_signals
                .emit(activated, &fired.session, &fired.id, timestamp);
        }
    }

    /// Ask the user whether an application may drive this machine.
    ///
    /// The same overlay the screen-share chooser uses, and deliberately so: it
    /// is the one piece of the desktop that already means "somebody is asking
    /// for something and you are about to answer", it already takes the
    /// keyboard away from whatever had it, and it already puts itself back.
    /// What differs is the sentence at the top and the device set underneath
    /// it, both of which travel in the same `screencast.pick` event.
    ///
    /// Three refusals before the question is ever asked, and each one is the
    /// safe direction:
    ///
    /// A chooser already on screen refuses, as it does for a share. Two
    /// dialogues and one keyboard is a race the person cannot see, and the one
    /// they lose here hands over the machine.
    ///
    /// No desktop page to draw with refuses too, and this is where it parts
    /// company with the screen-share path. That path falls back to sharing the
    /// focused window without asking, because a compositor running without its
    /// shell is a test or a crash and a portal that never works is worse than
    /// one that shares a window. The same reasoning does not survive being
    /// applied to a keyboard: a machine that grants control of itself because
    /// its own user interface is broken has turned a shell bug into a way in.
    ///
    /// And an application that asked to see the desk as well gets nothing if
    /// there is nothing to show it, rather than a grant with no picture — it
    /// asked for both, and half of it is not what it agreed to drive.
    fn open_remote_picker(
        &mut self,
        devices: u32,
        types: Option<u32>,
        reply: async_channel::Sender<Result<crate::screencast::remote::Started, String>>,
    ) {
        if self.picker.is_some() {
            let _ = reply.try_send(Err("something else is already being chosen".to_owned()));
            return;
        }
        if !self.shell_can_draw() {
            // Said at warn rather than info: this is a request for the
            // keyboard that was turned down for a reason that has nothing to
            // do with the person who would have answered it, and the only
            // place that can be seen is here.
            tracing::warn!(
                "no desktop page is drawing, so refusing to let an application drive this machine"
            );
            let _ = reply.try_send(Err("there is nobody to ask".to_owned()));
            return;
        }

        let sources = match types {
            Some(types) => {
                let sources = self.screencast_sources(types);
                if sources.is_empty() {
                    let _ = reply.try_send(Err("there is nothing to share".to_owned()));
                    return;
                }
                sources
            }
            // A session that drives without watching. The chooser is then a
            // plain yes or no, which `Picker::step` already handles: an empty
            // list has no highlight to move.
            None => Vec::new(),
        };

        tracing::info!(
            "an application is asking to drive the {}",
            crate::screencast::remote::device_names(devices).join(", ")
        );
        self.raise_picker(
            sources,
            devices,
            Vec::new(),
            String::new(),
            crate::screencast::Reply::Remote(reply),
        );
    }

    /// Whether there is an engine in this process to be sent input.
    ///
    /// Narrower than it sounds, and deliberately false on every shipped build:
    /// only the `wpe` backend runs the page inside the compositor, and only
    /// that one needs pointer and key events forwarded to it by hand. The
    /// out-of-process backends are Wayland clients and receive their own.
    ///
    /// Which makes this the wrong question to ask about *drawing*, and asking
    /// it was a bug — see `shell_can_draw`.
    pub fn shell_is_up(&self) -> bool {
        #[cfg(feature = "wpe")]
        {
            !self.shells.is_empty()
        }
        #[cfg(not(feature = "wpe"))]
        {
            false
        }
    }

    /// Whether there is a desktop page on screen, whichever backend draws it.
    ///
    /// The question anything the shell has to *show* must ask. `shell_is_up`
    /// is about input and is false for every backend except `wpe`, so the
    /// screen-share chooser — which asked it — never appeared on any shipped
    /// build: `packages.default` is `cef`, and every request fell through to
    /// the no-shell fallback and shared the focused window without asking.
    /// That is a screen handed over on the strength of a keystroke nobody
    /// made, which is exactly what the chooser exists to prevent.
    ///
    /// A committed buffer rather than a live process: a page that has started
    /// and not yet painted cannot show anything either, and a chooser sent to
    /// one is a share that hangs until the timeout.
    pub fn shell_can_draw(&self) -> bool {
        #[cfg(feature = "wpe")]
        if self
            .shells
            .iter()
            .any(|page| page.desktop && page.owned.is_some())
        {
            return true;
        }
        self.shell_clients
            .iter()
            .any(|page| page.desktop && page.owned.is_some())
    }

    /// Everything the application could be given a picture of.
    ///
    /// Windows before monitors, and the focused window first: what somebody
    /// means to share is usually what they were just looking at, and the list
    /// is walked from the top.
    ///
    /// The sources that name nothing in particular — the whole desk, the
    /// focused window, the active monitor — come at the end of the group they
    /// belong to rather than at the top of it. They are the more useful answer
    /// for a long meeting and the more surprising one for a quick share, and
    /// the top of the list is what Enter picks without reading.
    fn screencast_sources(&self, types: u32) -> Vec<crate::screencast::Source> {
        let mut sources = Vec::new();
        if types & crate::screencast::SOURCE_WINDOW != 0 {
            let focused = self
                .views
                .get(self.focused)
                .filter(|view| view.mapped && view.capture_allowed);
            if let Some(view) = focused {
                sources.push(crate::screencast::Source::Window(view.id));
            }
            for view in self.views.iter() {
                if view.mapped
                    && view.capture_allowed
                    && Some(view.id) != focused.map(|view| view.id)
                {
                    sources.push(crate::screencast::Source::Window(view.id));
                }
            }
            // Only when there is a window to follow. Offered on an empty
            // desktop it is a choice that shares a black rectangle until
            // somebody opens something.
            if focused.is_some() {
                sources.push(crate::screencast::Source::FollowWindow);
            }
        }
        if types & crate::screencast::SOURCE_MONITOR != 0 {
            // The one being looked at first, for the same reason.
            let active = self
                .active_output
                .as_ref()
                .and_then(|name| self.output_by_name(name));
            if let Some(output) = active.clone() {
                sources.push(crate::screencast::Source::Output(output));
            }
            for output in self.space.outputs() {
                if Some(output) != active.as_ref() {
                    sources.push(crate::screencast::Source::Output(output.clone()));
                }
            }
            // Both of these are the one monitor there is on a laptop, so they
            // are offered only where they differ from a row already in the
            // list. Two rows that share the same picture is a choice that is
            // not one.
            if self.space.outputs().count() > 1 {
                sources.push(crate::screencast::Source::AllOutputs);
                sources.push(crate::screencast::Source::FollowOutput);
            }
        }
        sources
    }

    /// Send the chooser, or what is left of it, to the shell.
    fn notify_picker(&mut self) {
        let Some(picker) = self.picker.as_ref() else {
            return;
        };
        let id = picker.id;
        // A shortcuts request is its own message: there is nothing to
        // highlight and nothing to step through, and the rows are chords
        // rather than things to look at. Sent once — the list cannot change
        // while it is up — where a share re-sends on every keypress.
        if !picker.shortcuts.is_empty() {
            let app = picker.app.clone();
            let shortcuts = picker
                .shortcuts
                .iter()
                .map(|shortcut| viewport_ipc::ShortcutRow {
                    id: shortcut.id.clone(),
                    description: shortcut.description.clone(),
                    trigger: shortcut.chord.clone(),
                })
                .collect();
            self.notify(&Event::ShortcutsPick { id, app, shortcuts });
            return;
        }
        let selected = picker.selected as u32;
        let devices = picker.devices;
        let sources = picker
            .sources
            .iter()
            .map(|source| match source {
                crate::screencast::Source::Output(output) => {
                    let properties = output.physical_properties();
                    viewport_ipc::CastSource {
                        kind: "output".to_owned(),
                        label: output.name(),
                        detail: format!("{} {}", properties.make, properties.model)
                            .trim()
                            .to_owned(),
                    }
                }
                crate::screencast::Source::Window(id) => {
                    let view = self.views.get(*id);
                    viewport_ipc::CastSource {
                        kind: "window".to_owned(),
                        label: view.map(|view| view.title()).unwrap_or_default(),
                        detail: view.map(|view| view.app_id()).unwrap_or_default(),
                    }
                }
                // Said in full here rather than left to the shell to name: the
                // difference between sharing a monitor and sharing whichever
                // monitor you are on is the whole of what somebody is agreeing
                // to, and it has to be readable in the row.
                crate::screencast::Source::AllOutputs => viewport_ipc::CastSource {
                    kind: "all-outputs".to_owned(),
                    label: "All monitors".to_owned(),
                    detail: format!("{} screens, side by side", self.space.outputs().count()),
                },
                crate::screencast::Source::FollowWindow => viewport_ipc::CastSource {
                    kind: "follow-window".to_owned(),
                    label: "The focused window".to_owned(),
                    detail: "follows as you switch windows".to_owned(),
                },
                crate::screencast::Source::FollowOutput => viewport_ipc::CastSource {
                    kind: "follow-output".to_owned(),
                    label: "The active monitor".to_owned(),
                    detail: "follows as you move between screens".to_owned(),
                },
            })
            .collect();

        let event = Event::ScreencastPick {
            id,
            sources,
            selected,
            // Empty for a plain screen share, which is what the shell reads to
            // decide which of the two questions it is asking.
            devices: crate::screencast::remote::device_names(devices),
        };
        self.notify(&event);
    }

    /// Move the highlight.
    pub fn step_screencast_pick(&mut self, delta: isize) {
        let Some(picker) = self.picker.as_mut() else {
            return;
        };
        picker.step(delta);
        self.notify_picker();
    }

    /// Agree to what is highlighted.
    ///
    /// The two kinds of chooser part company only here, at the answer. A
    /// screen share picks one row and starts a stream from it. A
    /// remote-desktop session grants the devices that were asked for — the
    /// whole set, because the chooser asks about the set and there is no row
    /// to leave out — and starts a stream as well if the application asked to
    /// watch as well as drive.
    ///
    /// Neither answer leaves from here when a stream was asked for: starting
    /// one no longer produces its node on the spot, so the reply is owed
    /// until PipeWire names it — `begin_cast` takes the reply and settles it
    /// from there. What still happens here is everything the chooser owes:
    /// the dialogue comes down, the keyboard goes back, and the shell is told
    /// the question was answered.
    ///
    /// Granting exactly what was asked for rather than less is worth being
    /// explicit about: the interface lets an implementation grant a subset,
    /// and a chooser that offered the devices one at a time would be a better
    /// dialogue. It is not what this one draws, and answering with a subset
    /// nobody chose would be inventing a decision.
    pub fn confirm_screencast_pick(&mut self) {
        let Some(picker) = self.picker.take() else {
            return;
        };
        let id = picker.id;
        let devices = picker.devices;
        let selected = picker.sources.into_iter().nth(picker.selected);
        let picker = crate::screencast::Answered {
            shortcuts: picker.shortcuts,
            restore: picker.restore,
            reply: picker.reply,
        };
        let mut shortcuts_answered = false;

        match picker.reply {
            crate::screencast::Reply::Cast(reply) => match selected {
                Some(source) => self.begin_cast(source, 0, crate::screencast::Reply::Cast(reply)),
                // A share with nothing selected is a chooser that had no rows,
                // which the screen-share path refuses before it ever gets
                // here. Answered rather than dropped all the same.
                None => {
                    let _ = reply.try_send(Err("nothing was chosen".to_owned()));
                }
            },
            crate::screencast::Reply::Remote(reply) => match selected {
                Some(source) => {
                    self.begin_cast(source, devices, crate::screencast::Reply::Remote(reply))
                }
                None => {
                    // A session that drives without watching: the grant is
                    // the whole answer, and nothing waits on PipeWire for it.
                    let _ = reply.try_send(Ok(crate::screencast::remote::Started {
                        devices,
                        cast: None,
                    }));
                }
            },
            crate::screencast::Reply::Shortcuts(reply) => {
                shortcuts_answered = true;
                match self.pending_shortcuts.take() {
                    Some((session, app_id)) => {
                        tracing::info!(
                            "{app_id:?} may hear {}",
                            crate::shortcuts::count(picker.shortcuts.len())
                        );
                        self.grant_shortcuts(&session, &app_id, &picker.shortcuts);
                        // Written down only now, so a refusal leaves no trace
                        // and the next launch asks again.
                        self.shortcuts_store.remember(&app_id, &picker.shortcuts);
                        let _ = reply.try_send(Ok(picker.shortcuts));
                    }
                    // The request went away while the dialogue was up. Nothing
                    // to grant it to, and nothing to write down.
                    None => {
                        let _ = reply.try_send(Err("the request is gone".to_owned()));
                    }
                }
            }
        }
        if shortcuts_answered {
            self.notify(&Event::ShortcutsPickDone { id });
        } else {
            self.notify(&Event::ScreencastPickDone { id });
        }
        self.restore_focus(picker.restore);
    }

    /// Agree to nothing, which the application is told is a refusal.
    pub fn cancel_screencast_pick(&mut self) {
        let Some(picker) = self.picker.take() else {
            return;
        };
        // Dropping the sender would answer too — the other end reads a closed
        // channel as no answer — but saying so keeps the reason in one place.
        self.pending_shortcuts = None;
        let shortcuts = !picker.shortcuts.is_empty();
        picker.reply.refuse("nothing was chosen");
        if shortcuts {
            self.notify(&Event::ShortcutsPickDone { id: picker.id });
        } else {
            self.notify(&Event::ScreencastPickDone { id: picker.id });
        }
        self.restore_focus(picker.restore);
    }

    /// Put the keyboard back where the chooser found it.
    ///
    /// A window that has closed in the meantime is left alone: focus stays
    /// nowhere, which is what it would have been anyway.
    fn restore_focus(&mut self, id: u32) {
        if self.views.get(id).is_some_and(|view| view.mapped) {
            crate::apply::focus_view(self, id);
        }
    }

    /// Start sharing one source, and owe the chooser its answer.
    ///
    /// The answer no longer leaves from here. The stream is started — which
    /// returns before PipeWire has named it, usually by a couple of
    /// milliseconds — so the reply waits on the name: the success goes out
    /// the moment it arrives, from the promise armed with
    /// [`crate::screencast::stream::Arrival::when_named`], and
    /// `finish_share` refuses it at the deadline if it never does. What this
    /// used to cost was the desktop itself: answering inline meant blocking
    /// the compositor's thread on the daemon, half a second at the ceiling,
    /// for a number that travels perfectly well on its own.
    ///
    /// `devices` rides along because a remote-desktop reply wraps the stream
    /// together with the grant, and both halves of that answer are known at
    /// different times; a plain screen share passes zero.
    fn begin_cast(
        &mut self,
        source: crate::screencast::Source,
        devices: u32,
        reply: crate::screencast::Reply,
    ) {
        let source_type = source.kind();
        // Written down before the share starts, because starting it takes the
        // source, and what a window is called has to be read while there is
        // still a window to ask.
        let remembered = self.remember_cast(&source);
        let begun = match self.start_cast(source) {
            Ok(begun) => begun,
            Err(e) => {
                reply.refuse(&e.to_string());
                return;
            }
        };
        let id = self.next_share;
        self.next_share += 1;

        // The success leaves from wherever the news arrives, which is the
        // PipeWire thread: everything it needs is owned data and a sender,
        // none of it the compositor's, so there is nothing to route back
        // through the event loop. The failure cannot leave from there —
        // refusing means tearing the stream back out of `casts`, and that
        // wants `&mut self` — which is what the deadline below is for.
        let (width, height) = (begun.size.w, begun.size.h);
        let success = reply.clone();
        // Both halves of the answer carry what was shared — the reply so the
        // application can ask again, and this record because the deadline
        // answers with it gone just as surely as the promise does with it
        // kept.
        let remembered_for_the_reply = remembered.clone();
        let name_for_the_log = begun.name.clone();
        begun.arrival.when_named(move |node| {
            tracing::info!("sharing {name_for_the_log} as pipewire node {node}");
            success.share(
                crate::screencast::portal::Started {
                    node,
                    width,
                    height,
                    source_type,
                    remembered: remembered_for_the_reply,
                },
                devices,
            );
        });

        // Armed either way, because the two ends of this meet nowhere: the
        // success announces itself from another thread, and only this thread
        // may clear what is recorded about the share. At the deadline it
        // settles the question one way or the other — refusal and teardown
        // if the node never came, the record swept up if it did.
        let _ = self.loop_handle.insert_source(
            smithay::reexports::calloop::timer::Timer::from_duration(
                crate::screencast::stream::NODE_TIMEOUT,
            ),
            move |_, _, state| {
                state.finish_share(id);
                smithay::reexports::calloop::timer::TimeoutAction::Drop
            },
        );

        self.pending_shares.push(PendingShare {
            id,
            arrival: begun.arrival,
            stream_id: begun.stream_id,
            name: begun.name,
            reply,
        });
    }

    /// Settle a share whose node was being waited on.
    ///
    /// Run once per share, at its deadline, whichever way it went. A named
    /// stream left only this record behind — its reply went out from the
    /// PipeWire thread the moment the node appeared — and the record goes
    /// now. Anything else is a share PipeWire never finished making: the
    /// refusal goes out from here, and the stream comes back out of
    /// `casts`, because a stream with no node is one no consumer can ever
    /// reach, and nobody but this deadline knows it exists.
    fn finish_share(&mut self, id: u64) {
        let Some(at) = self.pending_shares.iter().position(|share| share.id == id) else {
            return;
        };
        let share = self.pending_shares.remove(at);

        // Claiming the failure is what keeps the two answers from ever both
        // going out. It succeeds unless the node arrived first — in which
        // case the Ok reply is already gone, the stream is a working one,
        // and there is nothing here left to do but throw the record away.
        if !share.arrival.fail() {
            return;
        }
        tracing::warn!(
            "pipewire never named the stream for {}, refusing the share",
            share.name
        );
        share
            .reply
            .refuse("pipewire did not give the stream a node");

        let before = self.casts.len();
        self.casts.retain(|cast| cast.stream.id != share.stream_id);
        if self.casts.len() != before {
            tracing::info!("took the unnamed stream for {} back", share.name);
        }
        if self.casts.is_empty() && self.pending_shares.is_empty() {
            // As in `stop_cast`: nothing is being shared, so the connection
            // is not worth holding. The pending shares count alongside the
            // casts — each of them still holds a live stream of its own.
            self.pipewire = None;
        }
    }

    /// Say what is being shared in terms that outlive it.
    ///
    /// A window whose id names nothing is not written down: a token that
    /// restores to an empty app id and an empty title would match the first
    /// nameless window on the desk next time, which is a share of whatever
    /// happens to be open rather than of what was agreed to.
    fn remember_cast(
        &self,
        source: &crate::screencast::Source,
    ) -> Option<crate::screencast::Remembered> {
        use crate::screencast::{Remembered, Source};
        match source {
            Source::Output(output) => Some(Remembered::Output(output.name())),
            Source::Window(id) => {
                let view = self.views.get(*id)?;
                if !view.capture_allowed {
                    return None;
                }
                let (app_id, title) = (view.app_id(), view.title());
                (!app_id.is_empty() || !title.is_empty())
                    .then_some(Remembered::Window { app_id, title })
            }
            Source::AllOutputs => Some(Remembered::AllOutputs),
            Source::FollowWindow => Some(Remembered::FollowWindow),
            Source::FollowOutput => Some(Remembered::FollowOutput),
        }
    }

    /// Turn what was written down back into something to share, or nothing.
    ///
    /// Checked against what the application asked for as well as against what
    /// is on the desk: a token minted when a browser wanted either kind comes
    /// back when it wants only a window, and handing it a monitor because that
    /// is what it shared last time is a screen shared by a tab that asked for
    /// a tab.
    fn restore_source(
        &self,
        remembered: &crate::screencast::Remembered,
        types: u32,
    ) -> Option<crate::screencast::Source> {
        use crate::screencast::{Remembered, Source};
        if types & remembered.kind() == 0 {
            return None;
        }

        let source = match remembered {
            Remembered::Output(name) => Source::Output(self.output_by_name(name)?),
            Remembered::Window { app_id, title } => {
                Source::Window(self.remembered_window(app_id, title)?)
            }
            Remembered::AllOutputs => Source::AllOutputs,
            Remembered::FollowWindow => Source::FollowWindow,
            Remembered::FollowOutput => Source::FollowOutput,
        };
        // And that there is something behind it now. A following source with
        // nothing to follow starts a stream that would show a black rectangle
        // until somebody opened a window, which is worse than being asked.
        self.resolve_cast(&source).map(|_| source)
    }

    /// The window a remembered share meant, if it is still open.
    ///
    /// The choosing is `screencast::matching_window`, which is where it can be
    /// tested without a compositor around it.
    fn remembered_window(&self, app_id: &str, title: &str) -> Option<u32> {
        let open: Vec<_> = self
            .views
            .iter()
            .filter(|view| view.mapped && view.capture_allowed)
            .map(|view| crate::screencast::Open {
                id: view.id,
                app_id: view.app_id(),
                title: view.title(),
            })
            .collect();
        crate::screencast::matching_window(app_id, title, &open)
    }
}
