// SPDX-License-Identifier: GPL-3.0-or-later
//
// Output enumeration, modes, placement, adaptive sync and gamma.
// Included by `state.rs` to share the state module's imports and privacy.

impl ViewportState {
    /// Every physical head, including disabled heads and mirror sinks that are
    /// deliberately absent from the logical `Space`.
    pub fn physical_outputs(&self) -> Vec<Output> {
        if let Some(udev) = self.udev.as_ref() {
            let mut outputs: Vec<Output> = self.space.outputs().cloned().collect();
            let unmapped: Vec<Output> = udev
                .surfaces()
                .map(|surface| surface.output.clone())
                .filter(|output| !outputs.iter().any(|mapped| mapped == output))
                .collect();
            outputs.extend(unmapped);
            return outputs;
        }
        if let Some(headless) = self.headless.as_ref() {
            let mut outputs: Vec<Output> = self.space.outputs().cloned().collect();
            let unmapped: Vec<Output> = headless
                .outputs
                .values()
                .filter(|output| !outputs.iter().any(|mapped| mapped == *output))
                .cloned()
                .collect();
            outputs.extend(unmapped);
            return outputs;
        }
        self.space.outputs().cloned().collect()
    }

    pub fn output_is_enabled(&self, output: &Output) -> bool {
        self.udev
            .as_ref()
            .and_then(|udev| udev.surfaces().find(|surface| surface.output == *output))
            .map(|surface| surface.enabled)
            .unwrap_or_else(|| {
                self.headless.as_ref().is_some_and(|headless| {
                    headless.outputs.contains_key(&output.name())
                        && !headless.disabled.contains(&output.name())
                }) || self.space.outputs().any(|other| other == output)
            })
    }

    pub fn configured_vrr(&self, name: &str) -> viewport_ipc::event::VrrMode {
        self.output_vrr.get(name).copied().unwrap_or(if self.adaptive_sync {
            viewport_ipc::event::VrrMode::Always
        } else {
            viewport_ipc::event::VrrMode::Off
        })
    }

    pub fn desired_vrr(&self, target: &Output) -> bool {
        use smithay::reexports::wayland_protocols::wp::content_type::v1::server::wp_content_type_v1::Type;
        use smithay::wayland::compositor::{TraversalAction, with_surface_tree_downward};
        use smithay::wayland::content_type::ContentTypeSurfaceCachedState;

        let source = self.mirror_source(target);
        let geometry = self.space.output_geometry(&source);
        let mut fullscreen = false;
        let mut game_or_video = false;
        for view in self.views.iter().filter(|view| view.mapped && view.visible) {
            let Some(layout) = self.space.element_geometry(&view.window) else {
                continue;
            };
            if !geometry.is_some_and(|area| area.overlaps(layout)) {
                continue;
            }
            fullscreen |= view.wants_fullscreen()
                && geometry.is_some_and(|area| layout.contains_rect(area));
            if let Some(surface) = view.surface() {
                with_surface_tree_downward(
                    &surface,
                    (),
                    |_, _, _| TraversalAction::DoChildren(()),
                    |_, states, _| {
                        let mut state = states
                            .cached_state
                            .get::<ContentTypeSurfaceCachedState>();
                        game_or_video |=
                            matches!(state.current().content_type(), Type::Game | Type::Video);
                    },
                    |_, _, _| true,
                );
            }
        }
        crate::output_topology::vrr_effective(
            self.configured_vrr(&target.name()),
            fullscreen,
            game_or_video,
        )
    }

    /// Apply only when this physical target's desired state transitions.
    pub fn update_output_vrr(&mut self, output: &Output, wanted: bool) {
        let name = output.name();
        if self.output_vrr_wanted.get(&name) == Some(&wanted) {
            return;
        }
        let before = self.output_vrr_effective.get(&name).copied().unwrap_or(false);
        let Some(surface) = self
            .udev
            .as_mut()
            .and_then(|udev| udev.surfaces_mut().find(|surface| surface.output == *output))
        else {
            self.output_vrr_wanted.insert(name.clone(), wanted);
            self.output_vrr_effective.insert(name, false);
            return;
        };
        match surface
            .drm_output
            .with_compositor(|compositor| compositor.use_vrr(wanted))
        {
            Ok(()) => {
                let effective = surface
                    .drm_output
                    .with_compositor(|compositor| compositor.vrr_enabled());
                self.output_vrr_effective.insert(name.clone(), effective);
                self.output_vrr_wanted.insert(name.clone(), wanted);
                surface.clear_tearing_refusal();
                tracing::info!("{name}: VRR {}", if effective { "on" } else { "off" });
            }
            Err(e) => {
                self.output_vrr_wanted.insert(name.clone(), wanted);
                tracing::debug!("VRR unavailable on {name}: {e}");
            }
        }
        if self.output_vrr_effective.get(&name).copied().unwrap_or(false) != before {
            self.notify_output_layout();
        }
    }

    pub fn mirror_source(&self, output: &Output) -> Output {
        self.output_mirrors
            .get(&output.name())
            .and_then(|name| self.any_output_by_name(name))
            .unwrap_or_else(|| output.clone())
    }

    fn output_gpu(&self, name: &str) -> Option<usize> {
        self.udev.as_ref().and_then(|udev| {
            udev.outputs()
                .find(|(_, surface)| surface.output.name() == name)
                .map(|(id, _)| id.device)
        }).or_else(|| self.headless.as_ref().and_then(|h| h.outputs.contains_key(name).then_some(0)))
    }

    pub fn configure_mirror(&mut self, sink: &Output, source: Option<&str>) -> Result<(), String> {
        let sink_name = sink.name();
        let mut wanted = self.output_mirrors.clone();
        match source.filter(|name| !name.is_empty()) {
            Some(source) => { wanted.insert(sink_name.clone(), source.to_owned()); }
            None => { wanted.remove(&sink_name); }
        }
        let present: std::collections::HashSet<_> = self.physical_outputs().into_iter().map(|o| o.name()).collect();
        crate::output_topology::validate(&wanted, &present, |name| self.output_gpu(name))?;

        if let Some(source_name) = source.filter(|name| !name.is_empty()) {
            let source_output = self.any_output_by_name(source_name).ok_or_else(|| format!("mirror source {source_name} does not exist"))?;
            if !self.output_is_enabled(&source_output) || self.output_mirrors.contains_key(source_name) {
                return Err(format!("mirror source {source_name} is not an enabled logical output"));
            }
            let sink_size = sink.current_mode().map(|m| sink.current_transform().transform_size(m.size));
            let source_size = source_output.current_mode().map(|m| source_output.current_transform().transform_size(m.size));
            if sink_size != source_size
                || sink.current_scale().fractional_scale()
                    != source_output.current_scale().fractional_scale()
            {
                return Err(format!("mirror requires matching transformed mode and scale ({} differs from {source_name})", sink_name));
            }
            self.space.unmap_output(sink);
            self.drop_pending_copies_for(sink);
            if let Some(global) = self
                .udev
                .as_mut()
                .and_then(|udev| udev.surfaces_mut().find(|surface| surface.output == *sink))
                .and_then(|surface| surface.global.take())
                .or_else(|| {
                    self.headless
                        .as_mut()
                        .and_then(|headless| headless.globals.remove(&sink_name))
                })
            {
                self.display_handle.remove_global::<Self>(global);
            }
        } else if self.output_mirrors.contains_key(&sink_name) && self.output_is_enabled(sink) {
            let source = self.output_mirrors.get(&sink_name).and_then(|name| self.any_output_by_name(name));
            let location = source
                .as_ref()
                .and_then(|source| self.space.output_geometry(source))
                .map(|geo| (geo.loc.x + geo.size.w, geo.loc.y))
                .unwrap_or_default();
            self.map_output_at(sink, location);
            let global = sink.create_global::<Self>(&self.display_handle);
            if let Some(surface) = self
                .udev
                .as_mut()
                .and_then(|udev| udev.surfaces_mut().find(|surface| surface.output == *sink))
            {
                surface.global = Some(global);
            } else if let Some(headless) = self.headless.as_mut() {
                headless.globals.insert(sink_name.clone(), global);
            }
        }
        self.output_mirrors = wanted;
        self.refresh_eis_regions();
        self.needs_render = true;
        Ok(())
    }

    /// Repair topology after unplug. A surviving sink is promoted to the old
    /// source's logical position; other sinks follow it.
    pub fn output_removed(&mut self, name: &str) {
        let old_position = self.output_memory.get(name).map(|m| (m.x, m.y));
        let enabled: std::collections::HashSet<String> = self
            .physical_outputs()
            .into_iter()
            .filter(|output| output.name() != name && self.output_is_enabled(output))
            .map(|output| output.name())
            .collect();
        let promoted = crate::output_topology::remove(&mut self.output_mirrors, name, &enabled);
        self.output_vrr_wanted.remove(name);
        self.output_vrr_effective.remove(name);
        if let Some(output) = promoted.as_deref().and_then(|name| self.any_output_by_name(name)) {
            self.map_output_at(&output, old_position.unwrap_or_default());
            let global = output.create_global::<Self>(&self.display_handle);
            if let Some(surface) = self
                .udev
                .as_mut()
                .and_then(|udev| udev.surfaces_mut().find(|surface| surface.output == output))
            {
                surface.global = Some(global);
            } else if let Some(headless) = self.headless.as_mut() {
                headless.globals.insert(output.name(), global);
            }
            self.active_output = Some(output.name());
        }
    }

    pub fn heads(&self) -> Vec<crate::output_management::Head> {
        // Enabled outputs are the ones in the space; a disabled one keeps its
        // CRTC but is unmapped, because the shell places windows from the
        // layout and a monitor that is off has no place in it.
        let mut heads: Vec<crate::output_management::Head> = self
            .physical_outputs()
            .into_iter()
            .filter(|output| {
                self.output_is_enabled(output) && !self.output_mirrors.contains_key(&output.name())
            })
            .map(|output| crate::output_management::Head {
                output: output.clone(),
                enabled: true,
                position: self
                    .space
                    .output_geometry(&output)
                    .map(|geometry| geometry.loc)
                    .unwrap_or_default(),
                adaptive_sync: self.configured_vrr(&output.name())
                    != viewport_ipc::event::VrrMode::Off,
            })
            .collect();

        if let Some(udev) = self.udev.as_ref() {
            for surface in udev.surfaces().filter(|surface| {
                !surface.enabled && !self.output_mirrors.contains_key(&surface.output.name())
            }) {
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
                self.output_vrr.insert(
                    output.name(),
                    if vrr {
                        viewport_ipc::event::VrrMode::Always
                    } else {
                        viewport_ipc::event::VrrMode::Off
                    },
                );
                self.output_vrr_wanted.remove(&output.name());
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
        self.output_vrr_wanted.remove(&output.name());
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
        if self.output_is_enabled(output) == enabled {
            return;
        }
        self.output_vrr_wanted.remove(&output.name());

        if !enabled
            && self
                .output_mirrors
                .values()
                .any(|source| source == &output.name())
        {
            self.output_removed(&output.name());
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
            if !self.output_mirrors.contains_key(&output.name()) {
                self.map_output_at(output, location);
            }
            let needs_global = self
                .udev
                .as_ref()
                .and_then(|udev| udev.surfaces().find(|surface| surface.output == *output))
                .is_some_and(|surface| surface.global.is_none())
                || self
                    .headless
                    .as_ref()
                    .is_some_and(|headless| !headless.globals.contains_key(&output.name()));
            if needs_global && !self.output_mirrors.contains_key(&output.name()) {
                let global = output.create_global::<Self>(&self.display_handle);
                if let Some(surface) = self
                    .udev
                    .as_mut()
                    .and_then(|udev| udev.surfaces_mut().find(|surface| surface.output == *output))
                {
                    surface.global = Some(global);
                } else if let Some(headless) = self.headless.as_mut() {
                    headless.globals.insert(output.name(), global);
                }
            }
        } else {
            self.space.unmap_output(output);
            // Nothing will draw this screen while it is off, and screencopy
            // requests are served from the draw — so the ones waiting on it
            // are told now rather than left holding their buffers until the
            // housekeeping tick finds them. The tick still covers this; this
            // is just not making the client wait a second for the news.
            self.drop_pending_copies_for(output);
        }

        if let Some(headless) = self.headless.as_mut() {
            if enabled {
                headless.disabled.remove(&output.name());
            } else {
                headless.disabled.insert(output.name());
            }
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
