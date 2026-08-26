// SPDX-License-Identifier: GPL-3.0-or-later
//
// HDR, global adaptive sync and output enable controls.
// Included by `state.rs` to share the state module's imports and privacy.

impl ViewportState {
    pub fn hdr_enabled(&self, name: &str) -> bool {
        self.udev
            .as_ref()
            .map(|udev| {
                udev.surfaces()
                    .any(|surface| surface.output.name() == name && surface.hdr)
            })
            .unwrap_or(false)
    }

    /// Whether an output's display would accept HDR at all.
    ///
    /// Read from the connector rather than remembered, because it is a
    /// property of whatever is plugged in — the answer changes when the cable
    /// moves. A shell that offers the toggle on a display that cannot take it
    /// offers a key that does nothing.
    pub fn hdr_capable(&self, name: &str) -> bool {
        self.udev
            .as_ref()
            .and_then(|udev| {
                // By output rather than by surface, because the answer needs
                // the device as well as the connector: a connector handle is
                // device-local, so asking the primary about a monitor on the
                // second card reports a display that does support HDR as one
                // that does not — or answers from an unrelated connector that
                // happens to share the handle.
                let (id, connector) = udev
                    .outputs()
                    .find(|(_, surface)| surface.output.name() == name)
                    .map(|(id, surface)| (id, surface.connector))?;
                let device = udev.devices.get(id.device)?.manager.device();
                Some(crate::hdr::capable(device, connector))
            })
            .unwrap_or(false)
    }

    /// Switch an output into or out of HDR.
    ///
    /// Two properties on the connector, because Smithay's DRM backend has no
    /// notion of either. The client half — a client saying what its content
    /// actually is, and the renderer converting — is already here; without it
    /// this would only make every SDR window look washed out.
    pub fn set_hdr(&mut self, name: &str, enabled: bool) -> anyhow::Result<()> {
        let Some(udev) = self.udev.as_mut() else {
            anyhow::bail!("HDR needs the drm backend");
        };
        let Some((crtc, connector)) = udev
            .outputs()
            .find(|(_, surface)| surface.output.name() == name)
            .map(|(id, surface)| (id, surface.connector))
        else {
            anyhow::bail!("no such output");
        };

        // The card this screen is on. A connector handle means nothing on any
        // other, so asking the primary about a monitor plugged into the second
        // card either finds nothing — and reports a display that does support
        // HDR as one that does not — or finds an unrelated connector with the
        // same handle and turns HDR on for the wrong screen.
        let Some(gpu) = udev.devices.get_mut(crtc.device) else {
            anyhow::bail!("no such gpu");
        };
        let device = gpu.manager.device();
        if !crate::hdr::capable(device, connector) {
            anyhow::bail!("the display does not offer BT.2020 with PQ metadata");
        }
        crate::hdr::set(device, connector, enabled)?;

        if let Some(surface) = udev.surface_mut(crtc) {
            surface.hdr = enabled;
        }
        tracing::info!("{name}: HDR {}", if enabled { "on" } else { "off" });

        // The renderer converts into whatever the output is in, so it has to
        // be told what that now is — otherwise every window is reinterpreted
        // rather than converted, which is the washed-out look this exists to
        // avoid.
        let description = if enabled {
            viewport_vulkan::color::Description {
                primaries: viewport_vulkan::color::Primaries::BT2020,
                transfer: viewport_vulkan::color::TransferFunction::Pq,
                reference_luminance: 203.0,
            }
        } else {
            viewport_vulkan::color::Description::default()
        };
        // Not set here: the renderer has one output description and both
        // monitors draw through it, so setting it when a single display went
        // HDR converted everything on both — an SDR desktop reinterpreted as
        // PQ, which is the washed-out white the other screen showed. The
        // description belongs to whichever output is being drawn, so it is
        // set per frame in `udev::render` from that surface's own state.
        let _ = description;

        // The clients, too. They were told what this output was when they
        // connected and have no way to notice it changed, so a screen switched
        // into HDR goes on being drawn for by every one of them as though it
        // were SDR until it is said out loud.
        self.notify_output_colour(name);
        // And the shell, which draws the HDR badge from this.
        self.notify_output_layout();

        // Everything on screen was drawn for the old colour space.
        self.needs_render = true;
        Ok(())
    }

    /// Turn variable refresh on or off for every output that supports it.
    ///
    /// Whole-session rather than per-output, as in C (`src/output.c:315`): the
    /// config key is not under `outputs`, and a display that cannot do it says
    /// so rather than failing the commit.
    pub fn set_adaptive_sync(&mut self, enabled: bool) {
        let Some(udev) = self.udev.as_mut() else {
            return;
        };
        for surface in udev.surfaces_mut() {
            let result = surface
                .drm_output
                .with_compositor(|compositor| compositor.use_vrr(enabled));
            match result {
                Ok(()) => {
                    tracing::info!(
                        "adaptive sync {} on {}",
                        if enabled { "on" } else { "off" },
                        surface.output.name()
                    );
                    // The conditions a tearing refusal was measured under
                    // just changed, so the answer may have changed with it.
                    surface.clear_tearing_refusal();
                }
                // Not an error worth stopping for: most panels do not do it,
                // and asking is how you find out. Nothing changed, so nothing
                // is cleared.
                Err(e) => tracing::debug!(
                    "adaptive sync unavailable on {}: {e}",
                    surface.output.name()
                ),
            }
        }
    }

    /// Turn every output on or off.
    ///
    /// Blanking is a DRM state change rather than drawing black: a black frame
    /// still lights the panel, and the point is that the monitor sleeps.
    pub fn set_outputs_enabled(&mut self, enabled: bool) {
        let Some(udev) = self.udev.as_mut() else {
            return;
        };
        if udev.blanked == !enabled {
            return;
        }
        udev.blanked = !enabled;
        tracing::info!("outputs {}", if enabled { "on" } else { "off" });

        if enabled {
            // Nothing to undo explicitly: `clear` re-enables on the next
            // queued frame. But vblank cannot provide one — nothing has been
            // queued since the screens went off — so the frame has to be asked
            // for.
            for surface in udev.surfaces_mut() {
                surface.pending = false;
                surface.queued_at = None;
                // Everything that was on screen went with the blanking, and
                // the damage history does not know it. Without this the screen
                // comes back holding whatever last moved and nothing else.
                surface.drm_output.reset_buffers();
            }
            self.needs_render = true;
            return;
        }

        for surface in udev.surfaces_mut() {
            // DPMS off and every plane disabled, rather than a black frame: a
            // black frame still lights the panel, and the point is that the
            // monitor sleeps.
            if let Err(e) = surface
                .drm_output
                .with_compositor(|compositor| compositor.clear())
            {
                tracing::warn!("could not blank an output: {e}");
            }
            // No frame is in flight now, and none will be until the screens
            // come back. The clock on it goes too: the watchdog measures a
            // stall from `queued_at`, and a flip abandoned here is not a GPU
            // that stopped answering.
            surface.pending = false;
            surface.queued_at = None;
        }
    }
}
