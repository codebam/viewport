// SPDX-License-Identifier: GPL-3.0-or-later
//
// The compositor's half of windowsOut: deciding a close is worth fading,
// photographing the closing window while a renderer is in hand, and keeping
// the resulting ghosts until their fade is spent. Included into `state.rs`,
// so it shares the module with `read_window_pixels` and the view stack.

impl ViewportState {
    /// The close motion the config asks for, resolved through the windowsOut
    /// leaf's inheritance.
    pub(crate) fn ghost_motion(&self) -> crate::ghost::GhostMotion {
        crate::ghost::resolve_motion(self.config.motion.as_ref())
    }

    /// Whether a closing view should be photographed and faded rather than
    /// removed at once. A window nobody can see — minimized, hidden, never
    /// mapped, or one whose rule forbids capture — has no frame worth keeping;
    /// a locked session draws nothing through the lock; and motion off (a zero
    /// duration) is the instant removal it asks for.
    fn close_fade_eligible(&self, id: u32) -> bool {
        use smithay::wayland::seat::WaylandFocus as _;
        if self.locked || self.ghost_motion().duration.is_zero() {
            return false;
        }
        let Some(view) = self.views.get(id) else {
            return false;
        };
        view.mapped
            && view.visible
            && !view.minimized
            && view.capture_allowed
            && view.window.wl_surface().is_some()
    }

    /// Mark a closed view for a windowsOut fade. Returns true when the caller
    /// should leave the view mapped and in the stack — the render pass
    /// photographs it on the next frame and removes it then, so the window
    /// shows for that one frame and its ghost takes over at full opacity the
    /// frame after, with no blink between. False means remove it now, as
    /// before.
    pub(crate) fn defer_close_for_fade(&mut self, id: u32) -> bool {
        if !self.close_fade_eligible(id) {
            return false;
        }
        if !self.closing_views.contains(&id) {
            self.closing_views.push(id);
        }
        self.needs_render = true;
        true
    }

    /// Photograph every view owed a snapshot, turn each into a ghost, and
    /// remove it. Called from the render pass — the one place a renderer
    /// lives — before the frame is built, so a ghost is ready to draw on the
    /// frame after the window's last.
    pub(crate) fn snapshot_closing_views<R, B>(&mut self, renderer: &mut R)
    where
        R: smithay::backend::renderer::Renderer
            + smithay::backend::renderer::Bind<B>
            + smithay::backend::renderer::Offscreen<B>
            + smithay::backend::renderer::ExportMem
            + smithay::backend::renderer::ImportAll
            + smithay::backend::renderer::ImportMem
            + smithay::backend::renderer::ImportDma
            + crate::background_effect::BackgroundEffectRenderer,
        B: 'static,
        <R as smithay::backend::renderer::RendererSuper>::TextureId: Clone + Send + Sync + 'static,
        <R as smithay::backend::renderer::RendererSuper>::Error: Send + Sync + 'static,
    {
        let ids = std::mem::take(&mut self.closing_views);
        if ids.is_empty() {
            return;
        }
        let motion = self.ghost_motion();
        for id in ids {
            self.snapshot_one_ghost::<R, B>(id, motion, renderer);
        }
        if !self.ghosts.is_empty() {
            self.needs_render = true;
        }
    }

    /// One closing window: read its pixels back, keep them as a ghost, and
    /// remove the view whether or not the picture came out. A surface that
    /// died before the photograph — the client tore down faster than the
    /// frame — leaves no ghost, which is the instant removal this feature
    /// replaces, and no worse.
    fn snapshot_one_ghost<R, B>(
        &mut self,
        id: u32,
        motion: crate::ghost::GhostMotion,
        renderer: &mut R,
    ) where
        R: smithay::backend::renderer::Renderer
            + smithay::backend::renderer::Bind<B>
            + smithay::backend::renderer::Offscreen<B>
            + smithay::backend::renderer::ExportMem
            + smithay::backend::renderer::ImportAll
            + smithay::backend::renderer::ImportMem
            + smithay::backend::renderer::ImportDma
            + crate::background_effect::BackgroundEffectRenderer,
        B: 'static,
        <R as smithay::backend::renderer::RendererSuper>::TextureId: Clone + Send + Sync + 'static,
        <R as smithay::backend::renderer::RendererSuper>::Error: Send + Sync + 'static,
    {
        use smithay::utils::Physical;

        // The geometry comes from the shell's box, not the space's bbox: the
        // client destroying its toplevel unmaps the surface, and an unmapped
        // element has no bbox — but the box the shell drew is still the size
        // and place the window showed at, which is what the ghost fades from.
        let bbox = self.views.get(id).map(|view| {
            let b = view.box_;
            smithay::utils::Rectangle::<i32, smithay::utils::Logical>::new(
                (b.x, b.y).into(),
                (b.width, b.height).into(),
            )
        });
        let Some(bbox) = bbox.filter(|b| b.size.w > 0 && b.size.h > 0) else {
            self.remove_closed_view(id);
            return;
        };
        if self.locked {
            self.remove_closed_view(id);
            return;
        }
        let location = bbox.loc.to_f64();
        let drawn = bbox.size;
        let slide_dx = self.close_slide_distance(&bbox, motion.style);

        let mut captured: Option<(Vec<u8>, smithay::utils::Size<i32, Physical>)> = None;
        let result = self.read_window_pixels::<R, B, _>(id, renderer, |pixels, size| {
            captured = Some((pixels.to_vec(), size));
            Ok(())
        });
        self.remove_closed_view(id);
        if let Err(e) = result {
            tracing::debug!("windowsOut: no snapshot of view {id}: {e}");
            return;
        }
        let Some((pixels, size)) = captured else {
            return;
        };
        if size.w <= 0 || size.h <= 0 {
            return;
        }
        tracing::debug!(
            "windowsOut: view {id} fades over {}ms ({:?})",
            motion.duration.as_millis(),
            motion.style
        );
        self.ghosts.push(crate::ghost::Ghost::new(
            &pixels, size, location, drawn, motion, slide_dx,
        ));
    }

    /// How far a slide carries a closing window: off the nearer horizontal
    /// edge of the output it was on, the direction the shell's entrance slide
    /// comes from reversed. Zero for a style that does not slide, or a window
    /// on no output.
    fn close_slide_distance(
        &self,
        bbox: &smithay::utils::Rectangle<i32, smithay::utils::Logical>,
        style: crate::ghost::OutStyle,
    ) -> f64 {
        if !matches!(style, crate::ghost::OutStyle::Slide | crate::ghost::OutStyle::Slidefade) {
            return 0.0;
        }
        let Some(output) = self.space.outputs().find(|o| {
            self.space
                .output_geometry(o)
                .map(|g| g.overlaps(*bbox))
                .unwrap_or(false)
        }) else {
            return 0.0;
        };
        let Some(g) = self.space.output_geometry(output) else {
            return 0.0;
        };
        let centre = bbox.loc.x + bbox.size.w / 2;
        let out_centre = g.loc.x + g.size.w / 2;
        if centre < out_centre {
            -((bbox.loc.x + bbox.size.w - g.loc.x) as f64)
        } else {
            (g.loc.x + g.size.w - bbox.loc.x) as f64
        }
    }

    /// The removal the close handler deferred: unmap and forget. The window's
    /// other teardown — foreign-toplevel closed, keyboard focus, the shell's
    /// `view.removed` — already happened when it closed; only its place in the
    /// stack and the view list waited for the photograph.
    fn remove_closed_view(&mut self, id: u32) {
        if let Some(view) = self.views.get(id) {
            let window = view.window.clone();
            self.space.unmap_elem(&window);
        }
        self.views.remove(id);
        self.needs_render = true;
    }

    /// Drop ghosts whose fade is spent. Called by the frame builder, which
    /// also asks for another frame while any remain.
    pub(crate) fn expire_ghosts(&mut self) {
        self.ghosts
            .retain(|ghost| ghost.sample(ghost.started.elapsed()).is_some());
        if !self.ghosts.is_empty() {
            self.needs_render = true;
        }
    }

    /// The ghosts that fall on this output, as draw descriptions: expired ones
    /// dropped, the rest sampled at their current fade and made output-relative
    /// and physical, the way a window's placement is. A locked session draws
    /// none — the lock screen is the only thing on screen.
    pub(crate) fn ghosts_for(
        &mut self,
        output_geometry: smithay::utils::Rectangle<i32, smithay::utils::Logical>,
        scale: f64,
    ) -> Vec<crate::render::GhostDraw> {
        self.expire_ghosts();
        if self.locked {
            return Vec::new();
        }
        let mut draws = Vec::new();
        for ghost in &self.ghosts {
            let Some(sample) = ghost.sample(ghost.started.elapsed()) else {
                continue;
            };
            let bbox = smithay::utils::Rectangle::<i32, smithay::utils::Logical>::new(
                ghost.location.to_i32_round(),
                ghost.size,
            );
            if !output_geometry.overlaps(bbox) {
                continue;
            }
            draws.push(crate::render::GhostDraw {
                buffer: ghost.buffer.clone(),
                location: (ghost.location - output_geometry.loc.to_f64()).to_physical(scale),
                size: ghost.size,
                src: ghost.src,
                id: ghost.id.clone(),
                alpha: sample.alpha,
                scale: sample.scale,
                dx: sample.dx * scale,
            });
        }
        draws
    }
}