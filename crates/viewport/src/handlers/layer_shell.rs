// SPDX-License-Identifier: GPL-3.0-or-later
//
// wlr-layer-shell. Ports src/layer_shell.c.
//
// The one shell protocol Viewport genuinely needs from outside, and the reason
// is that it is not a layout protocol. A launcher, a notification daemon, a
// lock screen's backdrop — these are not windows the shell arranges, they are
// surfaces that ask for a layer and a strip of the edge, and the answer is the
// same whatever the layout is. So unlike xdg-shell, this is the compositor's
// to answer, and the shell is not consulted.
//
// What it does have to tell the shell is the usable area: an exclusive zone
// takes space away from where windows may go, and only the shell places
// windows.

use smithay::backend::renderer::utils::with_renderer_surface_state;
use smithay::desktop::{layer_map_for_output, LayerSurface, PopupKind};
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::compositor::get_parent;
use smithay::wayland::shell::wlr_layer::{
    Layer, LayerSurface as WlrLayerSurface, WlrLayerShellHandler, WlrLayerShellState,
};

use crate::state::ViewportState;

impl WlrLayerShellHandler for ViewportState {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: WlrLayerSurface,
        wl_output: Option<WlOutput>,
        _layer: Layer,
        namespace: String,
    ) {
        // The output the client named, the active one, or the first there is.
        // A client may legitimately not care, and there is always somewhere to
        // put it as long as an output exists at all.
        let output = wl_output
            .as_ref()
            .and_then(Output::from_resource)
            .or_else(|| {
                self.active_output
                    .as_deref()
                    .and_then(|n| self.output_by_name(n))
            })
            .or_else(|| self.space.outputs().next().cloned());
        let Some(output) = output else {
            // Nothing to attach it to. Closing says so rather than leaving the
            // client waiting for a configure that cannot be computed.
            tracing::warn!("layer surface {namespace:?} arrived with no output to put it on");
            surface.send_close();
            return;
        };

        tracing::debug!("layer surface {namespace:?} on {}", output.name());
        let layer = LayerSurface::new(surface, namespace);
        let (policy, _) = crate::layer::apply(&layer, &self.layer_rules);
        log_policy(&layer, policy);
        let mut map = layer_map_for_output(&output);
        if let Err(e) = map.map_layer(&layer) {
            tracing::warn!("could not map a layer surface: {e}");
            return;
        }
        drop(map);
        crate::layer::set_owner(&layer, output.clone());

        // An exclusive zone changes where windows may go, and the shell is
        // what puts them there.
        self.notify_output_layout();
        self.needs_render = true;
    }

    fn layer_destroyed(&mut self, surface: WlrLayerSurface) {
        crate::layer::clear_owner(surface.wl_surface());
        let outputs = self.physical_outputs();
        let found = outputs.iter().find_map(|output| {
            let map = layer_map_for_output(output);
            let layer = map
                .layers()
                .find(|layer| layer.layer_surface() == &surface)
                .cloned();
            layer.map(|layer| (output.clone(), layer))
        });
        let Some((output, layer)) = found else {
            let mut cleaned = false;
            for output in outputs {
                let mut map = layer_map_for_output(&output);
                let before = map.len();
                map.cleanup();
                cleaned |= map.len() != before;
            }
            if cleaned {
                self.notify_output_layout();
                self.needs_render = true;
                self.refresh_pointer_focus();
            }
            return;
        };
        let was_wallpaper = layer.layer() == Layer::Background;
        let mut map = layer_map_for_output(&output);
        map.unmap_layer(&layer);
        map.arrange();
        drop(map);

        // The wallpaper program has gone — swaybg killed, hyprpaper restarting
        // — so the terminal can have the position back. Checked rather than
        // assumed: a second wallpaper client on another monitor is still one,
        // and this runs after the unmap so what it counts is what is left.
        if was_wallpaper {
            self.background_reclaim_wallpaper(&output);
        }

        // The space it reserved is usable again.
        self.notify_output_layout();
        self.needs_render = true;
        self.refresh_pointer_focus();
    }

    fn new_popup(
        &mut self,
        parent: WlrLayerSurface,
        surface: smithay::wayland::shell::xdg::PopupSurface,
    ) {
        // A launcher's completion list. Tracked like any other popup; it is
        // positioned against its parent, which is not a window this compositor
        // has a rectangle for.
        if let Err(error) = self.popups.track_popup(PopupKind::Xdg(surface.clone())) {
            tracing::warn!("could not track a layer popup: {error}");
            return;
        }
        if crate::layer::inherit_owner(surface.wl_surface(), parent.wl_surface()) {
            crate::layer::register_popup(&surface);
        }
    }
}

impl ViewportState {
    /// Re-resolve every mapped layer in place after a config reload.
    pub fn refresh_layer_policies(&mut self) {
        let mut mapped = 0usize;
        let mut changed = 0usize;
        let mut stacking_changed = false;
        // Disabled and mirror outputs are absent from Space but retain their
        // layer maps. Refresh them now so re-enabling one cannot revive stale
        // capture policy.
        for output in self.physical_outputs() {
            let map = layer_map_for_output(&output);
            for layer in map.layers() {
                mapped += 1;
                let previous = crate::layer::policy(layer, &self.layer_rules);
                let (policy, did_change) = crate::layer::apply(layer, &self.layer_rules);
                if did_change {
                    changed += 1;
                    stacking_changed |= previous.z_index != policy.z_index;
                    log_policy(layer, policy);
                }
            }
        }
        tracing::debug!("layer rules: refreshed {mapped} mapped surfaces; {changed} changed");
        self.needs_render = true;
        if stacking_changed {
            self.refresh_pointer_focus();
        }
    }

    /// Size a layer surface and send its configure, once it has committed.
    ///
    /// Called from the commit handler: a layer surface has no size until it
    /// has been arranged, and it will not paint until it has been configured.
    pub fn layer_commit(&mut self, surface: &WlSurface) {
        let mut owner_surface = surface.clone();
        while let Some(parent) = get_parent(&owner_surface) {
            owner_surface = parent;
        }
        let Some((output, layer)) = crate::layer::owner(&owner_surface) else {
            return;
        };
        let is_root = layer.wl_surface() == surface;

        let sent = is_root
            && smithay::wayland::compositor::with_states(surface, |states| {
                states
                    .data_map
                    .get::<smithay::wayland::shell::wlr_layer::LayerSurfaceData>()
                    .map(|data| data.lock().unwrap().initial_configure_sent)
                    .unwrap_or(true)
            });

        let mut map = layer_map_for_output(&output);
        // Arrange first, so the size the client asked for is respected before
        // it is told what it got.
        let changed = is_root && map.arrange();
        // Arranging does not send the first configure, and a layer surface
        // will not paint before it has one. wmenu asks for width 0 — meaning
        // "the compositor decides" — so without this it allocates a
        // zero-width buffer and the connection dies on an invalid shm pool.
        if is_root && !sent {
            layer.layer_surface().send_configure();
        }
        let has_buffer =
            with_renderer_surface_state(layer.wl_surface(), |state| state.buffer().is_some())
                .unwrap_or(false);
        let drawable_changed = crate::layer::set_drawable(&layer, has_buffer, &self.layer_rules);
        let protocol_layer_changed = crate::layer::update_protocol_layer(&layer, &self.layer_rules);
        let surface_hit_state_changed =
            crate::layer::hit_test_state_changed(surface, crate::layer::popup_geometry(surface));
        let pointer_stack_changed =
            drawable_changed || protocol_layer_changed || surface_hit_state_changed;
        drop(map);

        // A wallpaper program — swaybg, hyprpaper — asks for the background
        // layer, which is drawn over the terminal this compositor can draw as
        // the wallpaper. Two things claiming the same position is one of them
        // painting for nothing, so the terminal stands down.
        //
        // Here rather than where the layer surface was created, because a
        // surface exists before it has painted and may never paint at all: a
        // wallpaper client that died between asking for the layer and drawing
        // on it took the terminal with it and left the desktop blank for the
        // rest of the session, since the position is only offered back when a
        // layer is destroyed. `wallpaper_layer_on` is what counts a mapped one.
        if is_root && self.wallpaper_layer_on(&output) {
            self.background_yield_to_wallpaper(&output);
        }

        if changed {
            self.notify_output_layout();
        }
        self.needs_render = true;
        if changed || pointer_stack_changed {
            self.refresh_pointer_focus();
        }
    }

    /// Give a layer surface the keyboard if it asked for it.
    ///
    /// A launcher is unusable without this: it asks for exclusive
    /// interactivity precisely so that typing reaches it rather than whatever
    /// was focused before. `OnDemand` is left to the click that focuses it,
    /// and `None` never takes focus at all.
    pub fn focus_layer_if_exclusive(&mut self, surface: &WlSurface) {
        use smithay::wayland::shell::wlr_layer::KeyboardInteractivity;

        let wants = self.space.outputs().any(|output| {
            layer_map_for_output(output)
                .layer_for_surface(surface, smithay::desktop::WindowSurfaceType::TOPLEVEL)
                .map(|layer| {
                    layer.cached_state().keyboard_interactivity == KeyboardInteractivity::Exclusive
                })
                .unwrap_or(false)
        });
        if !wants {
            return;
        }

        if let Some(keyboard) = self.seat.get_keyboard() {
            // Already there, and nothing to do: re-setting the focus re-sends
            // the enter and re-announces it, and with two exclusive layers
            // committing on the same frame the last one would win for no
            // reason — the surface that has the keyboard keeps it.
            let already = keyboard
                .current_focus()
                .map(|focus| focus.is_surface(surface))
                .unwrap_or(false);
            if already {
                return;
            }
            let serial = smithay::utils::SERIAL_COUNTER.next_serial();
            keyboard.set_focus(self, Some(surface.clone().into()), serial);
        }
        // The shell draws focus rings from this, and a layer surface is not
        // one of its views — so it has to hear that no view holds focus now.
        self.notify_focus(crate::views::NO_VIEW);
    }

    /// The part of an output windows may use, after exclusive zones.
    ///
    /// A bar that reserved the top of the screen has taken that space away
    /// from the shell, which is the only thing that places windows — so this
    /// is what `output.layout` has to carry rather than the whole rectangle.
    pub fn usable_area(
        &self,
        output: &Output,
    ) -> smithay::utils::Rectangle<i32, smithay::utils::Logical> {
        let geometry = self.space.output_geometry(output).unwrap_or_default();
        let mut usable = layer_map_for_output(output).non_exclusive_zone();
        // The map works in output-local coordinates; the shell works in the
        // layout's.
        usable.loc += geometry.loc;
        usable
    }
}

fn log_policy(layer: &LayerSurface, policy: crate::layer::Policy) {
    tracing::debug!(
        "layer surface {:?}: policy opacity={} capture={} blur={} z_index={}",
        layer.namespace(),
        policy.opacity,
        policy.capture,
        policy.blur,
        policy.z_index
    );
}
