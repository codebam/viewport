// SPDX-License-Identifier: GPL-3.0-or-later
//
// Output enumeration, modes, placement, adaptive sync and gamma.
// Included by `state.rs` to share the state module's imports and privacy.

impl ViewportState {
    pub fn heads(&self) -> Vec<crate::output_management::Head> {
        // Enabled outputs are the ones in the space; a disabled one keeps its
        // CRTC but is unmapped, because the shell places windows from the
        // layout and a monitor that is off has no place in it.
        let mut heads: Vec<crate::output_management::Head> = self
            .space
            .outputs()
            .map(|output| crate::output_management::Head {
                output: output.clone(),
                enabled: true,
                position: self
                    .space
                    .output_geometry(output)
                    .map(|geometry| geometry.loc)
                    .unwrap_or_default(),
                adaptive_sync: self.adaptive_sync,
            })
            .collect();

        if let Some(udev) = self.udev.as_ref() {
            for surface in udev.surfaces().filter(|surface| !surface.enabled) {
                heads.push(crate::output_management::Head {
                    output: surface.output.clone(),
                    enabled: false,
                    position: Point::default(),
                    adaptive_sync: false,
                });
            }
        }
        heads
    }

    /// Tell every output-management client what the outputs are now.
    ///
    /// Deliberately not called from `notify_output_layout`: that fires when a
    /// layer surface changes the usable area, which is not an output change,
    /// and every call invalidates the configurations clients are holding.
    pub fn advertise_outputs(&mut self) {
        let heads = self.heads();
        let dh = self.display_handle.clone();
        self.output_management_state.advertise::<Self>(&dh, &heads);
    }

    /// Carry out — or check — what a client asked of the outputs.
    ///
    /// Everything is validated before anything is changed. A configuration is
    /// one operation to the client that sent it, and half of it applied is a
    /// layout nobody asked for: the monitor moved and the resolution refused.
    pub fn apply_output_configuration(
        &mut self,
        changes: &[crate::output_management::HeadChange],
        test_only: bool,
    ) -> bool {
        use std::collections::HashSet;

        let mut still_on: HashSet<String> = self
            .heads()
            .into_iter()
            .filter(|head| head.enabled)
            .map(|head| head.output.name())
            .collect();

        for change in changes {
            let Some(output) = self.any_output_by_name(&change.name) else {
                tracing::warn!("output configuration names {}, which is gone", change.name);
                return false;
            };
            if change.enabled {
                still_on.insert(change.name.clone());
            } else {
                still_on.remove(&change.name);
            }

            if let Some(mode) = change.mode {
                if mode.size.w <= 0 || mode.size.h <= 0 {
                    return false;
                }
                // A mode the display never offered cannot be programmed on
                // real hardware: the kernel takes a modeline from the
                // connector's own list. Nested has no such constraint, so a
                // custom mode is only refused where it would actually fail.
                let known = output.modes().contains(&mode);
                if !known && self.udev.is_some() {
                    tracing::warn!(
                        "{}: {}x{}@{} is not a mode this display offers",
                        change.name,
                        mode.size.w,
                        mode.size.h,
                        mode.refresh
                    );
                    return false;
                }
            }
            if change.scale.is_some_and(|scale| scale <= 0.0) {
                return false;
            }
        }

        // A session with every screen off cannot be turned back on from
        // inside it. Refusing is the only thing that leaves the user a way
        // back.
        if still_on.is_empty() {
            tracing::warn!("refusing a configuration that would turn every output off");
            return false;
        }

        if test_only {
            return true;
        }

        for change in changes {
            let Some(output) = self.any_output_by_name(&change.name) else {
                continue;
            };
            if !change.enabled {
                self.set_output_enabled(&output, false);
                continue;
            }
            self.set_output_enabled(&output, true);

            if let Some(mode) = change.mode {
                self.set_output_mode(&output, mode);
            }
            if change.transform.is_some() || change.scale.is_some() {
                let scale = change.scale.map(smithay::output::Scale::Fractional);
                output.change_current_state(None, change.transform, scale, None);
                self.output_reshaped(&output);
            }
            if let Some(position) = change.position {
                self.map_output_at(&output, (position.x, position.y));
            }
            if let Some(vrr) = change.adaptive_sync {
                self.set_output_adaptive_sync(&output, vrr);
            }
            // `wlr-randr` and the display panel of a settings app arrange
            // monitors through here rather than through `output.configure`, and
            // an arrangement made with one of those is worth restoring too.
            self.remember_output(&output);
        }

        // Put every window that should be on screen back in the space.
        //
        // A window is in the space because the shell placed it there, and the
        // shell places from the layout — so a monitor coming back leaves any
        // window that was on it in whatever state it was left in, and nothing
        // re-sends a rectangle for a window whose rectangle has not changed.
        // The shell keeps drawing its frame either way, which is what a
        // re-enabled output showing borders and no windows was.
        self.remap_placed_views();

        self.notify_output_layout();
        self.advertise_outputs();
        self.needs_render = true;
        true
    }

    /// Every view the shell has placed and not hidden belongs in the space.
    ///
    /// Idempotent: mapping an element that is already mapped at the same
    /// position is what `Space::map_element` does anyway, so this can be run
    /// after anything that may have taken windows out.
    pub fn remap_placed_views(&mut self) {
        let placed: Vec<(smithay::desktop::Window, (i32, i32))> = self
            .views
            .iter()
            .filter(|view| view.mapped && view.visible && view.placed)
            .map(|view| (view.window.clone(), (view.box_.x, view.box_.y)))
            .collect();
        let count = placed.len();
        for (window, location) in placed {
            self.space.map_element(window, location, false);
        }
        // Same as a layout: mapping restacks, so focus decides what is on top,
        // and the floats stay above whatever that is.
        if let Some(window) = self.views.get(self.focused).map(|view| view.window.clone()) {
            self.space.raise_element(&window, false);
        }
        self.restack();
        // Said out loud because "the windows did not come back" and "the
        // windows came back somewhere off screen" look identical from a chair
        // in front of the monitor.
        tracing::info!(
            "re-placed {count} view(s); the space holds {}",
            self.space.elements().count()
        );
        // A re-placement is a move between screens as far as a taskbar is
        // concerned — an output that went dark took its windows with it.
        self.sync_foreign_outputs();
    }

    /// Put an output at a position, in the layout and in what clients are told.
    ///
    /// `Space::map_output` alone moves the output for the compositor's own
    /// layout and leaves `wl_output.geometry` saying whatever it said before,
    /// which for every output here was the `(0, 0)` it was created at. A client
    /// asking where the monitors are then gets them all stacked on the origin.
    ///
    /// There is no xdg-output global to paper over it either, so `wl_output` is
    /// the only answer a client has. mpv reads it to work out which screen it is
    /// on and where to go fullscreen; with two monitors both claiming the origin
    /// it picks by the accident of enumeration order.
    pub fn map_output_at(&mut self, output: &Output, location: impl Into<Point<i32, Logical>>) {
        let location = location.into();
        self.space.map_output(output, location);
        output.change_current_state(None, None, None, Some(location));
        // And anything remote that was told where the monitors are. A libei
        // client points in the layout's own coordinates, which it was handed a
        // description of when its devices were made — see
        // `crate::libei::ViewportState::refresh_eis_regions`. This is the one
        // call every rearrangement goes through, and it costs nothing at all
        // when nobody is connected, which is nearly always.
        self.refresh_eis_regions();
    }

    /// Program a mode on the hardware, not only in the description of it.
    ///
    /// `change_current_state` alone moves what every client is told and leaves
    /// the CRTC scanning out what it was: the windows resize and the picture
    /// does not.
    fn set_output_mode(&mut self, output: &Output, mode: smithay::output::Mode) {
        output.change_current_state(Some(mode), None, None, None);

        let Some(udev) = self.udev.as_mut() else {
            // Nested, where the mode is the host window's to decide.
            return;
        };
        let Some((id, connector)) = udev
            .outputs()
            .find(|(_, surface)| surface.output == *output)
            .map(|(crtc, surface)| (crtc, surface.connector))
        else {
            return;
        };

        // The kernel takes a modeline from the connector's own list rather
        // than numbers, so the one it offered has to be found again.
        //
        // Asked of the device this output is on. A connector handle is
        // device-local, exactly as a crtc handle is, and `id` names the device
        // because that is what makes it meaningful — so looking one up on the
        // primary is asking the wrong card about a connector it does not have.
        // What that gives is either a lookup that fails, and a mode change
        // that silently does nothing, or a handle that happens to be valid
        // there too and describes a different monitor entirely.
        use smithay::reexports::drm::control::Device as _;
        let Some(gpu) = udev.devices.get_mut(id.device) else {
            return;
        };
        let device = gpu.manager.device();
        let Ok(info) = device.get_connector(connector, false) else {
            return;
        };
        let Some(drm_mode) = info
            .modes()
            .iter()
            .copied()
            .find(|candidate| smithay::output::Mode::from(*candidate) == mode)
        else {
            tracing::warn!("{}: the display no longer offers that mode", output.name());
            return;
        };

        let Some(device) = udev.devices.get_mut(id.device) else {
            return;
        };
        let Some(surface) = device.surfaces.get_mut(&id.crtc) else {
            return;
        };
        // No render elements: this is a modeset, and the frame after it is
        // drawn by the ordinary loop. Passing the current ones would only
        // matter for keeping other outputs lit through a bandwidth
        // renegotiation, and they are redrawn a moment later anyway.
        let result = crate::with_gpu!(&mut device.renderer, |renderer| surface
            .drm_output
            .use_mode(
                drm_mode,
                renderer,
                &smithay::backend::drm::output::DrmOutputRenderElements::<
                    _,
                    crate::render::OutputElement<_>,
                >::new(),
            )
            .map_err(|e| e.to_string()));
        match result {
            Ok(()) => {
                tracing::info!(
                    "{}: {}x{}@{}",
                    output.name(),
                    mode.size.w,
                    mode.size.h,
                    mode.refresh
                );
                // The mode is half of what a tearing refusal was measured
                // under, so the answer may have changed with it.
                surface.clear_tearing_refusal();
            }
            Err(e) => tracing::warn!("{}: the display refused the mode: {e}", output.name()),
        }
        // A modeset invalidates what was queued for this output.
        surface.pending = false;

        // And a different mode is a different screen, so the layer map and the
        // damage history are as stale as they are after a rotation.
        self.output_reshaped(output);
    }

    /// Turn one output on or off.
    ///
    /// The surface and its CRTC are kept either way, so coming back is a commit
    /// rather than a re-scan of the device. Off means the planes are cleared
    /// rather than painted black: a black frame still lights the panel.
    pub(crate) fn set_output_enabled(&mut self, output: &Output, enabled: bool) {
        let mapped = self.space.outputs().any(|other| other == output);
        if enabled == mapped {
            let already = self
                .udev
                .as_ref()
                .map(|udev| {
                    udev.surfaces()
                        .find(|surface| surface.output == *output)
                        .map(|surface| surface.enabled == enabled)
                        .unwrap_or(true)
                })
                .unwrap_or(true);
            if already {
                return;
            }
        }

        if enabled {
            // Back where it was, from the memory — an unmapped output has no
            // geometry of its own to read. Without an entry there it goes to
            // the right of everything, which is where a newly plugged monitor
            // goes too.
            let location = self
                .output_memory
                .get(&output.name())
                .map(|remembered| (remembered.x, remembered.y))
                .unwrap_or_else(|| {
                    let x = self
                        .space
                        .outputs()
                        .filter_map(|other| self.space.output_geometry(other))
                        .map(|geometry| geometry.loc.x + geometry.size.w)
                        .max()
                        .unwrap_or(0);
                    (x, 0)
                });
            self.map_output_at(output, location);
        } else {
            self.space.unmap_output(output);
            // Nothing will draw this screen while it is off, and screencopy
            // requests are served from the draw — so the ones waiting on it
            // are told now rather than left holding their buffers until the
            // housekeeping tick finds them. The tick still covers this; this
            // is just not making the client wait a second for the news.
            self.drop_pending_copies_for(output);
        }

        let Some(udev) = self.udev.as_mut() else {
            return;
        };
        let Some(surface) = udev
            .surfaces_mut()
            .find(|surface| surface.output == *output)
        else {
            return;
        };
        surface.enabled = enabled;
        surface.pending = false;
        if enabled {
            // The damage history describes a screen that has since been
            // cleared, so the next frame would redraw only what changed while
            // it was off — which for a still desktop is nothing, and the
            // monitor comes back showing the wallpaper with no windows on it.
            surface.drm_output.reset_buffers();
        }
        if !enabled {
            if let Err(e) = surface
                .drm_output
                .with_compositor(|compositor| compositor.clear())
            {
                tracing::warn!("could not switch {} off: {e}", output.name());
            }
        }
        tracing::info!("{} {}", output.name(), if enabled { "on" } else { "off" });
    }

    /// Variable refresh on one output.
    fn set_output_adaptive_sync(&mut self, output: &Output, enabled: bool) {
        let Some(udev) = self.udev.as_mut() else {
            return;
        };
        let Some(surface) = udev
            .surfaces_mut()
            .find(|surface| surface.output == *output)
        else {
            return;
        };
        match surface
            .drm_output
            .with_compositor(|compositor| compositor.use_vrr(enabled))
        {
            Ok(()) => {
                tracing::info!(
                    "adaptive sync {} on {}",
                    if enabled { "on" } else { "off" },
                    output.name()
                );
                // The conditions a tearing refusal was measured under just
                // changed, so the answer may have changed with it.
                surface.clear_tearing_refusal();
            }
            // Most panels cannot, and asking is how you find out. Nothing
            // changed, so nothing is cleared.
            Err(e) => tracing::debug!("adaptive sync unavailable on {}: {e}", output.name()),
        }
    }

    /// How many gamma entries this output's CRTC takes.
    ///
    /// `None` where there is no CRTC to ask — the nested backend, or a driver
    /// that offers no ramp — which tells a night-light client to skip this
    /// monitor rather than wait for a ramp that will never take.
    pub fn output_gamma_size(&mut self, output: &Output) -> Option<u32> {
        use smithay::reexports::drm::control::Device as _;

        let udev = self.udev.as_ref()?;
        let id = udev.id_of(output)?;
        let length = udev
            .devices
            .get(id.device)?
            .manager
            .device()
            .get_crtc(id.crtc)
            .ok()?
            .gamma_length();
        (length > 0).then_some(length)
    }

    /// Put a ramp on an output, or take one off.
    ///
    /// The legacy ioctl rather than the atomic GAMMA_LUT property: it is one
    /// call that does not have to join the commit putting a frame on screen,
    /// and a gamma change that waited for a page flip would be a colour shift
    /// that only lands when something moves.
    pub fn set_output_gamma(&mut self, output: &Output, ramp: Option<&crate::gamma::Ramp>) -> bool {
        let name = output.name();
        match ramp {
            Some(ramp) => {
                self.gamma_ramps.insert(name.clone(), ramp.clone());
            }
            None => {
                self.gamma_ramps.remove(&name);
            }
        }

        let Some(size) = self.output_gamma_size(output) else {
            return false;
        };
        let identity;
        let ramp = match ramp {
            Some(ramp) => ramp,
            None => {
                // Straight through, which is what a display with no client
                // looking after it should show. Leaving the last ramp in place
                // means a night-light client that was killed leaves the screen
                // orange until the next reboot.
                identity = crate::gamma::identity(size as usize);
                &identity
            }
        };
        self.apply_gamma(output, ramp)
    }

    fn apply_gamma(&mut self, output: &Output, ramp: &crate::gamma::Ramp) -> bool {
        use smithay::reexports::drm::control::Device as _;

        let Some(udev) = self.udev.as_ref() else {
            return false;
        };
        let Some(id) = udev.id_of(output) else {
            return false;
        };
        let Some(device) = udev.devices.get(id.device) else {
            return false;
        };
        match device
            .manager
            .device()
            .set_gamma(id.crtc, &ramp.red, &ramp.green, &ramp.blue)
        {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!("{}: the gamma ramp was refused: {e}", output.name());
                false
            }
        }
    }

    /// Put every ramp back after a VT switch.
    ///
    /// The kernel resets gamma when the session is handed over, and the client
    /// that set it has no way to know that happened — so coming back from
    /// another VT would drop the screen out of night mode until the next time
    /// wlsunset happened to recalculate.
    pub fn restore_gamma(&mut self) {
        let ramps: Vec<(String, crate::gamma::Ramp)> = self
            .gamma_ramps
            .iter()
            .map(|(name, ramp)| (name.clone(), ramp.clone()))
            .collect();
        for (name, ramp) in ramps {
            let Some(output) = self.any_output_by_name(&name) else {
                continue;
            };
            self.apply_gamma(&output, &ramp);
        }
    }
}
