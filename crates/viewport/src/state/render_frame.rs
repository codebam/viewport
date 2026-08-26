// SPDX-License-Identifier: GPL-3.0-or-later
//
// Assembly of one output frame and its cursor elements.
// Included by `state.rs` to share the state module's imports and privacy.

impl ViewportState {
    pub fn frame_for(&mut self, output: &Output) -> crate::render::Frame {
        use smithay::wayland::seat::WaylandFocus as _;
        use smithay::wayland::shell::wlr_layer::Layer;

        let Some(output_geometry) = self.space.output_geometry(output) else {
            return crate::render::Frame::default();
        };
        let scale = output.current_scale().fractional_scale();

        // Layer surfaces, split by whether they sit above the windows or
        // below them, in output-local physical coordinates.
        let (mut layers_above, mut layers_below) = (Vec::new(), Vec::new());
        {
            let map = smithay::desktop::layer_map_for_output(output);
            for layer in map.layers() {
                let Some(geometry) = map.layer_geometry(layer) else {
                    continue;
                };
                let location = geometry.loc.to_f64().to_physical(scale).to_i32_round();
                let entry = (layer.clone(), location);
                match layer.layer() {
                    Layer::Overlay | Layer::Top => layers_above.push(entry),
                    Layer::Background | Layer::Bottom => layers_below.push(entry),
                }
            }
        }

        // Front to back, which is the order the renderer draws in and the
        // order `Frame::windows` is documented to be in. Smithay's space
        // yields the other way round — bottom of the stack first — so taking
        // it as it comes drew the stack inside out: whatever had just been
        // raised went to the back. Two windows that never overlap look
        // identical either way, which is why a tiling desktop hid this and a
        // floating or maximised window over a tiled one did not.
        let windows: Vec<_> = self
            .space
            .elements()
            .rev()
            .filter_map(|window| {
                let layout = self.space.element_geometry(window)?;
                // Off this output entirely: drawing it would cost a texture
                // bind for something wholly clipped away.
                if !output_geometry.overlaps(layout) {
                    return None;
                }
                let view = window
                    .wl_surface()
                    .as_deref()
                    .and_then(|surface| self.views.find_by_surface(surface));
                let clip = view.and_then(|view| view.clip).map(|clip| {
                    Rectangle::<i32, Logical>::new(
                        (clip.x, clip.y).into(),
                        (clip.width, clip.height).into(),
                    )
                });
                // Which output the shell drew this window on, kept before the
                // placement shadows it: the frame below is only drawn on that
                // one. See `render::frame_on_output`.
                let drawn_on_this_output = crate::render::frame_on_output(clip, output_geometry);
                let (location, clip) =
                    crate::render::window_placement(window, layout, output_geometry, clip, scale);

                // The shell's border for this window, where it has said one
                // has to be drawn above whatever is underneath — as four
                // sides around the hole rather than one rectangle over it.
                //
                // The ids are the view's own, and there is nowhere else to get
                // them: a border side is one element frame after frame, and an
                // element whose id changes is an element the damage tracker
                // believes is new. A window with no view has no border to
                // draw, so this is also the only branch that needs any — four
                // fresh ids were minted per window per output per frame for
                // the case that never reaches them.
                let overlay: Vec<_> = view
                    .filter(|_| drawn_on_this_output)
                    .and_then(|view| {
                        view.frame
                            .map(|frame| (frame, view.box_, view.scale, &view.overlay_ids))
                    })
                    .map(|(frame, hole, drawn_at, overlay_ids)| {
                        crate::render::border_sides(frame, hole, drawn_at)
                            .into_iter()
                            .zip(overlay_ids.iter().cloned())
                            .filter_map(|(side, id)| {
                                // Held to this output: see `overlay_side` for
                                // what happens to a border that is not.
                                let local = crate::render::overlay_side(side, output_geometry)?;
                                Some((id, local.to_f64().to_physical(scale).to_i32_round()))
                            })
                            .collect()
                    })
                    .unwrap_or_default();

                // Where the window's own corner is, as opposed to where its
                // surface starts: the difference is the shadow a client draws
                // outside its geometry, and it is what a thumbnail has to be
                // scaled about.
                let origin = (layout.loc - output_geometry.loc)
                    .to_f64()
                    .to_physical(scale)
                    .to_i32_round();

                // The corner the shell drew, in physical pixels on this
                // output. Some windows have none: a fullscreen one, where the
                // stylesheet takes the border and the radius off and a rounded
                // video would be four notches of wallpaper in the corners of
                // the monitor, and a window smart radius has squared for the
                // same reason.
                let border = self.config.border.as_ref();
                // Square when the shell says so — smart radius, which is the
                // shell's call because only it knows the window is alone on
                // its workspace — or when the window is fullscreen, which the
                // stylesheet also draws without a border or a corner.
                let square = view.is_some_and(|view| view.square || view.wants_fullscreen());
                let radius = if square {
                    0
                } else {
                    border
                        .and_then(|border| border.radius)
                        .unwrap_or(crate::config::DEFAULT_BORDER_RADIUS)
                };
                // How much tighter the client's own corner is than the frame
                // around it. Configured, because the frame's thickness is.
                let width = border
                    .and_then(|border| border.width)
                    .unwrap_or(crate::config::DEFAULT_BORDER_WIDTH);
                let physical = |logical: i32| (f64::from(logical) * scale).round() as i32;
                // The box the shell drew, before any thumbnail scale: the
                // element rounding it is wrapped by the one that shrinks it,
                // so a corner is described once at full size.
                let box_ = smithay::utils::Rectangle::<i32, Physical>::new(
                    origin,
                    layout.size.to_f64().to_physical(scale).to_i32_round(),
                );
                let rounded = (radius > width).then(|| (box_, physical(radius - width)));

                // The corners of that border, which the sides above do not
                // reach: the curve crosses into the hole, and inside the hole
                // the shell is behind whatever this window is floating over.
                // Only for a rounded window — a square one's border is the
                // four sides and nothing else.
                //
                // The wedges the border's curve occupies inside the hole, and
                // not the corner squares that hold them: the rest of each
                // square is the hole itself, which in the shell's buffer is
                // the desktop's own background. Drawing that over the window a
                // floating one is lifted above puts four triangles of
                // wallpaper through it — and a client that does not fill its
                // hole to the pixel, which is every terminal, leaves room for
                // exactly that.
                let corners = view
                    .filter(|_| drawn_on_this_output && radius > width)
                    .and_then(|view| {
                        view.frame
                            .map(|frame| (frame, view.box_, view.scale, &view.corner_id))
                    })
                    .and_then(|(frame, hole, drawn_at, corner_id)| {
                        let hole = crate::render::drawn_hole_of(hole, drawn_at);
                        let hole = crate::render::overlay_side(hole, output_geometry)?;
                        let hole = hole.to_f64().to_physical(scale).to_i32_round();
                        let wedges = crate::rounded::cutaway(hole, physical(radius - width));
                        // Held inside the frame's own outer arc. The wedge is
                        // a copy of the shell's buffer, and with a radius much
                        // past the border's width the hole's square corner
                        // pokes *outside* the rounded frame — where the buffer
                        // is not border but whatever the page drew behind the
                        // frame, which over another window is the wallpaper.
                        // That was three or four pixels of it at each corner.
                        let frame = crate::render::overlay_side(frame, output_geometry)?;
                        let frame = frame.to_f64().to_physical(scale).to_i32_round();
                        let wedges = crate::rounded::clip_to(
                            wedges,
                            &crate::rounded::bands_within(frame, physical(radius)),
                        );
                        (!wedges.is_empty()).then(|| (corner_id.clone(), wedges))
                    });

                // The outside of the same corner, for the border sides drawn
                // above the windows underneath a floating one.
                let overlay_rounded = (radius > 0 && !overlay.is_empty())
                    .then(|| {
                        view.and_then(|view| view.frame).map(|frame| {
                            let frame = smithay::utils::Rectangle::<i32, Logical>::new(
                                (
                                    frame.x - output_geometry.loc.x,
                                    frame.y - output_geometry.loc.y,
                                )
                                    .into(),
                                (frame.width, frame.height).into(),
                            );
                            (
                                frame.to_f64().to_physical(scale).to_i32_round(),
                                physical(radius),
                            )
                        })
                    })
                    .flatten();

                Some(crate::render::WindowFrame {
                    window: window.clone(),
                    location,
                    origin,
                    clip,
                    rounded,
                    overlay_rounded,
                    // Both were stored and neither was ever applied: the
                    // overview drew its thumbnails and the compositor painted
                    // full-size windows into them, and a window faded out by
                    // the shell stayed solid.
                    scale: view.map(|view| view.scale).unwrap_or(1.0),
                    opacity: view.map(|view| view.opacity).unwrap_or(1.0),
                    overlay,
                    corners,
                })
            })
            .collect();

        // How many popups are about to be drawn, said when it changes. A menu
        // that is created, configured and then drawn zero times is a
        // different fault from one that is drawn somewhere unhelpful.
        //
        // Only when something is listening. The census walks every popup of
        // every window and keys its tally by `output.name()`, which allocates
        // — all of it per output per frame, to decide whether to emit a line
        // that a session at the default level discards.
        if tracing::enabled!(tracing::Level::DEBUG) {
            use smithay::desktop::PopupManager;
            use smithay::wayland::seat::WaylandFocus as _;
            let popups: usize = windows
                .iter()
                .filter_map(|frame| frame.window.wl_surface())
                .map(|surface| PopupManager::popups_for_surface(&surface).count())
                .sum();
            // Per output: one monitor drawing a menu and the other not is the
            // ordinary case, and a single counter flapped between them once a
            // frame.
            let seen = self.popups_drawn.entry(output.name()).or_default();
            if popups != *seen {
                *seen = popups;
                tracing::debug!("popup: {popups} being drawn on {}", output.name());
            }
        }

        let cursor = self.cursor_for(output, output_geometry, scale);

        // Whichever backend painted it: a DMA-BUF, a rectangle in the layout
        // and a damage bag, which is all the element below needs. It does not
        // care whether WebKit handed the buffer over through an engine call or
        // a client attached it to a surface.
        //
        // Every page is placed by its own rectangle rather than at the layout's
        // origin. A desktop on its own spans the layout and the two are the
        // same; a `--url` page given one screen is not.
        let placed = |buffer: &smithay::backend::allocator::dmabuf::Dmabuf,
                      region: smithay::utils::Rectangle<i32, Logical>,
                      damage: smithay::backend::renderer::utils::DamageSnapshot<
            i32,
            smithay::utils::Buffer,
        >,
                      id: smithay::backend::renderer::element::Id| {
            crate::render::Shell {
                buffer: buffer.clone(),
                location: (
                    (region.loc.x - output_geometry.loc.x) as f64 * scale,
                    (region.loc.y - output_geometry.loc.y) as f64 * scale,
                )
                    .into(),
                damage,
                id,
            }
        };

        // Both backends' pages, in one list: only one of them is ever running.
        let mut drawn: Vec<(bool, crate::render::Shell)> = Vec::new();
        #[cfg(feature = "wpe")]
        for page in &self.shells {
            if let Some((buffer, _)) = page.owned.as_ref() {
                drawn.push((
                    page.desktop,
                    placed(
                        buffer,
                        page.region,
                        page.damage.snapshot(),
                        page.element_id.clone(),
                    ),
                ));
            }
        }
        for page in &self.shell_clients {
            if let Some((buffer, _)) = page.owned.as_ref() {
                drawn.push((
                    page.desktop,
                    placed(
                        buffer,
                        page.region,
                        page.damage.snapshot(),
                        page.element_id.clone(),
                    ),
                ));
            }
        }
        // The desktop page is the one the overlays are cropped out of, so it is
        // the one that goes in `shell`; the rest are drawn beside it, under
        // everything, at their own corners.
        let mut shell = None;
        let mut pages = Vec::new();
        for (desktop, element) in drawn {
            if desktop && shell.is_none() {
                shell = Some(element);
            } else {
                pages.push(element);
            }
        }

        // Whether the desktop page's rectangle covers this whole screen, for
        // the lock screen alone. Every other use of the shell's buffer is
        // happy to cover part of a monitor and leave the rest to the clear
        // colour; a lock screen is not, because the part it does not cover is
        // the part the desktop would show through.
        #[allow(unused_mut)]
        let mut shell_covers_output = self.shell_clients.iter().any(|page| {
            page.desktop && page.owned.is_some() && page.region.contains_rect(output_geometry)
        });
        #[cfg(feature = "wpe")]
        {
            shell_covers_output |= self.shells.iter().any(|page| {
                page.desktop && page.owned.is_some() && page.region.contains_rect(output_geometry)
            });
        }

        // This monitor's wallpaper terminal, at this monitor's own origin.
        //
        // Not placed like the shell, which is one buffer across the layout and
        // is offset by where the output starts in it. There is a terminal per
        // screen, each configured to that screen's size, so each one begins at
        // the corner of the screen it belongs to.
        let background = self
            .background_surface_for(output)
            .cloned()
            .map(|surface| (surface, Point::from((0, 0))));

        // The part of the shell that goes above the windows, in this output's
        // own physical coordinates.
        let visible = smithay::utils::Rectangle::<i32, Logical>::from_size(
            (output_geometry.size.w, output_geometry.size.h).into(),
        );
        let overlay: Vec<_> = self
            .shell_overlays
            .iter()
            .enumerate()
            .filter_map(|(at, rect)| {
                let local = smithay::utils::Rectangle::<i32, Logical>::new(
                    (
                        rect.loc.x - output_geometry.loc.x,
                        rect.loc.y - output_geometry.loc.y,
                    )
                        .into(),
                    rect.size,
                );
                // Nothing of it on this monitor: a notification is drawn on one
                // of them and the others carry on as they were.
                local.intersection(visible)?;
                let id = self.shell_overlay_ids.get(at)?.clone();
                Some((id, local.to_f64().to_physical(scale).to_i32_round()))
            })
            .collect();

        crate::render::Frame {
            layers_above,
            windows,
            layers_below,
            shell,
            pages,
            background,
            overlay,
            cursor,
            // The magnified region, on the output the pointer is on and no
            // other. `output_geometry` rather than the output's own mode: the
            // magnifier works in the layout's logical coordinates, which is
            // what the pointer is in and what the hit test is in, and being
            // in the same space as those is the whole reason nothing else has
            // to be told about it.
            magnify: self.magnified_view(output_geometry),
            scale,
            // Nothing is locked on nearly every frame this compositor ever
            // draws, and `output.name()` allocates a `String` to ask — so the
            // empty map is answered without building the key for it.
            lock: (!self.lock_surfaces.is_empty())
                .then(|| self.lock_surfaces.get(&output.name()))
                .flatten()
                // A locker that exited leaves its surfaces behind until the
                // next housekeeping tick; drawing one is drawing nothing.
                .filter(|lock| smithay::utils::IsAlive::alive(lock.wl_surface()))
                .map(|lock| lock.wl_surface().clone()),
            locked_blank: self.locked,
            // Drawn only where all three hold: the session is locked with the
            // built-in screen, the shell has drawn one for *this* lock and
            // painted since saying so, and its rectangle covers this monitor.
            // Anything short of all three is a black screen, which is the side
            // to fail on.
            shell_lock: self.locked
                && self.lock_mode.is_built_in()
                && shell_covers_output
                && self.lock_screen_is_drawing(),
        }
    }

    /// The pointer image for an output, resolved but not imported.
    fn cursor_for(
        &mut self,
        output: &Output,
        output_geometry: Rectangle<i32, Logical>,
        scale: f64,
    ) -> crate::render::Cursor {
        use smithay::input::pointer::CursorImageStatus;

        let _ = output;
        // Still for long enough that the deadline took it away. Before the
        // client's own image is looked at, because the setting is about the
        // pointer being on the screen at all — a text field's I-beam parked
        // over a film is the same thing it is there to remove.
        if self.cursor_hide.hidden() {
            return crate::render::Cursor::Hidden;
        }
        let Some(pointer) = self.seat.get_pointer() else {
            return crate::render::Cursor::Hidden;
        };
        let at = pointer.current_location();
        if !output_geometry.to_f64().contains(at) {
            return crate::render::Cursor::Hidden;
        }
        let local = (at - output_geometry.loc.to_f64()).to_physical(scale);

        let status =
            crate::cursor::active_image(self.tablet_cursor_status.as_ref(), &self.cursor_status);

        match status {
            CursorImageStatus::Hidden => crate::render::Cursor::Hidden,
            CursorImageStatus::Surface(surface) => {
                let hotspot = smithay::wayland::compositor::with_states(&surface, |states| {
                    states
                        .data_map
                        .get::<std::sync::Mutex<smithay::input::pointer::CursorImageAttributes>>()
                        // Read, not obeyed: a panic in whichever client thread
                        // held this last says nothing about the hotspot, and
                        // this runs every frame — a poisoned lock here would be
                        // a permanent cursor, not a diagnosis.
                        .map(|attrs| attrs.lock().unwrap_or_else(|e| e.into_inner()).hotspot)
                        .unwrap_or_default()
                });
                // The surface is drawn at the pointer minus its hotspot, and
                // `build` subtracts the hotspot — so this carries the pointer
                // position folded in.
                let at = local.to_i32_round();
                crate::render::Cursor::Surface(
                    surface,
                    hotspot.to_f64().to_physical(scale).to_i32_round() - at,
                )
            }
            CursorImageStatus::Named(shape) => {
                let millis = self.start_time.elapsed().as_millis() as u32;
                match self
                    .cursor_theme
                    .image(shape.name(), scale.ceil() as i32, millis)
                {
                    Some(image) => {
                        let at = local.to_i32_round() - image.hotspot;
                        crate::render::Cursor::Image(image, at)
                    }
                    None => {
                        if !self.cursor_warned {
                            self.cursor_warned = true;
                            tracing::warn!(
                                "no xcursor image for {:?}; set XCURSOR_THEME to a theme that is installed",
                                shape.name()
                            );
                        }
                        crate::render::Cursor::Hidden
                    }
                }
            }
        }
    }
}
