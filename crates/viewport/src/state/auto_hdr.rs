// SPDX-License-Identifier: GPL-3.0-or-later
//
// Automatic HDR: Hyprland's `render:cm_auto_hdr`.
//
// A fullscreen client that says its content is HDR gets the screen it is on
// switched into HDR for as long as it is there, and the screen goes back to
// what it was when the window leaves. Only a switch this code made is undone:
// a display somebody put in HDR by hand stays in it after a game closes.
//
// The check runs from the loop, not from the commit handler that noticed a
// window change state: switching a screen to HDR is two atomic commits on the
// connector, and that is not something to do in the middle of a client's
// commit.

impl ViewportState {
    /// Recompute which outputs should be in HDR for a fullscreen window.
    ///
    /// Cheap when nothing changes — a map lookup per screen — because it runs
    /// on every turn that could have moved a window in or out of fullscreen,
    /// not only on the ones that did.
    pub fn sync_auto_hdr(&mut self) {
        self.auto_hdr_pending = false;

        let mut wanted: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
        if self.cm_enabled && self.cm_auto_hdr > 0 && self.udev.is_some() {
            for view in self.views.iter() {
                // Only a mapped, visible, fullscreen window covers its whole
                // output, which is the one case the switch is for.
                if !view.mapped || view.minimized || !view.visible || !view.wants_fullscreen() {
                    continue;
                }
                let Some(name) = self.output_covering(view.box_) else {
                    continue;
                };
                if !self.hdr_capable(&name) || !self.view_wants_hdr(view.id) {
                    continue;
                }
                wanted.insert(name, view.id);
            }
        }

        // Put back the screens this code switched, now that nothing asks for
        // them any more.
        let release: Vec<String> = self
            .auto_hdr
            .keys()
            .filter(|name| !wanted.contains_key(*name))
            .cloned()
            .collect();
        for name in release {
            self.auto_hdr.remove(&name);
            if let Err(e) = self.set_hdr(&name, false) {
                tracing::warn!("{name}: could not leave HDR: {e:#}");
            }
        }

        // And switch on the ones that do ask. A screen already in HDR was put
        // there by somebody else, so it is left alone and stays in HDR after
        // the window goes — which is what "only undo what we did" means.
        for (name, id) in wanted {
            if self.auto_hdr.contains_key(&name) || self.hdr_enabled(&name) {
                continue;
            }
            match self.set_hdr(&name, true) {
                Ok(()) => {
                    tracing::info!(
                        "{name}: HDR for fullscreen view {id} (render.cm_auto_hdr {})",
                        self.cm_auto_hdr
                    );
                    if self.cm_auto_hdr == 2 {
                        tracing::info!(
                            "render.cm_auto_hdr 2 asks for the display's EDID primaries; HDR here is BT.2020"
                        );
                    }
                    self.auto_hdr.insert(name, id);
                }
                Err(e) => tracing::warn!("{name}: could not switch to HDR for view {id}: {e:#}"),
            }
        }
    }

    /// The output whose geometry covers the middle of this box.
    fn output_covering(&self, box_: viewport_ipc::geometry::Box) -> Option<String> {
        let centre: smithay::utils::Point<f64, smithay::utils::Logical> = (
            f64::from(box_.x) + f64::from(box_.width) / 2.0,
            f64::from(box_.y) + f64::from(box_.height) / 2.0,
        )
            .into();
        self.space.outputs().find_map(|output| {
            let geometry = self.space.output_geometry(output)?.to_f64();
            geometry.contains(centre).then(|| output.name())
        })
    }

    /// Whether this window's content asks for HDR.
    ///
    /// An absolute transfer function — PQ or HLG — is the declaration that a
    /// buffer is HDR. Wide primaries with an sRGB curve are wide-gamut SDR and
    /// do not need the screen switched.
    fn view_wants_hdr(&self, id: u32) -> bool {
        use smithay::wayland::seat::WaylandFocus as _;
        if self.view_denies_auto_hdr(id) {
            return false;
        }
        let Some(view) = self.views.get(id) else {
            return false;
        };
        let Some(surface) = view.window.wl_surface() else {
            return false;
        };
        viewport_vulkan::color::description_for(&surface)
            .transfer
            .is_absolute()
    }

    /// Whether a `no_auto_hdr` rule claims this window.
    fn view_denies_auto_hdr(&self, id: u32) -> bool {
        let Some(view) = self.views.get(id) else {
            return false;
        };
        crate::config::rule_flag(
            self.config.rules.as_ref(),
            "no_auto_hdr",
            &view.app_id(),
            &view.title(),
            view.tag.as_deref(),
        ) == Some(true)
    }
}
