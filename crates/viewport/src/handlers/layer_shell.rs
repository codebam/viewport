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

use smithay::desktop::{layer_map_for_output, LayerSurface, PopupKind};
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
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
        layer: Layer,
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
        let mut map = layer_map_for_output(&output);
        if let Err(e) = map.map_layer(&LayerSurface::new(surface, namespace)) {
            tracing::warn!("could not map a layer surface: {e}");
            return;
        }
        drop(map);

        // A wallpaper program — swaybg, hyprpaper — asks for the background
        // layer, which is drawn over the terminal this compositor can draw as
        // the wallpaper. Two things claiming the same position is one of them
        // painting for nothing, so the terminal stands down.
        if layer == Layer::Background {
            self.background_yield_to_wallpaper();
        }

        // An exclusive zone changes where windows may go, and the shell is
        // what puts them there.
        self.notify_output_layout();
        self.needs_render = true;
    }

    fn layer_destroyed(&mut self, surface: WlrLayerSurface) {
        let found = self.space.outputs().find_map(|output| {
            let map = layer_map_for_output(output);
            let layer = map
                .layers()
                .find(|layer| layer.layer_surface() == &surface)
                .cloned();
            layer.map(|layer| (output.clone(), layer))
        });
        let Some((output, layer)) = found else {
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
            self.background_reclaim_wallpaper();
        }

        // The space it reserved is usable again.
        self.notify_output_layout();
        self.needs_render = true;
    }

    fn new_popup(
        &mut self,
        _parent: WlrLayerSurface,
        surface: smithay::wayland::shell::xdg::PopupSurface,
    ) {
        // A launcher's completion list. Tracked like any other popup; it is
        // positioned against its parent, which is not a window this compositor
        // has a rectangle for.
        let _ = self.popups.track_popup(PopupKind::Xdg(surface));
    }
}

impl ViewportState {
    /// Size a layer surface and send its configure, once it has committed.
    ///
    /// Called from the commit handler: a layer surface has no size until it
    /// has been arranged, and it will not paint until it has been configured.
    pub fn layer_commit(&mut self, surface: &WlSurface) {
        let Some(output) = self.space.outputs().find(|output| {
            layer_map_for_output(output)
                .layer_for_surface(surface, smithay::desktop::WindowSurfaceType::TOPLEVEL)
                .is_some()
        }) else {
            return;
        };
        let output = output.clone();

        let sent = smithay::wayland::compositor::with_states(surface, |states| {
            states
                .data_map
                .get::<smithay::wayland::shell::wlr_layer::LayerSurfaceData>()
                .map(|data| data.lock().unwrap().initial_configure_sent)
                .unwrap_or(true)
        });

        let mut map = layer_map_for_output(&output);
        // Arrange first, so the size the client asked for is respected before
        // it is told what it got.
        let changed = map.arrange();
        // Arranging does not send the first configure, and a layer surface
        // will not paint before it has one. wmenu asks for width 0 — meaning
        // "the compositor decides" — so without this it allocates a
        // zero-width buffer and the connection dies on an invalid shm pool.
        if !sent {
            if let Some(layer) =
                map.layer_for_surface(surface, smithay::desktop::WindowSurfaceType::TOPLEVEL)
            {
                layer.layer_surface().send_configure();
            }
        }
        drop(map);

        if changed {
            self.notify_output_layout();
        }
        self.needs_render = true;
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
            let serial = smithay::utils::SERIAL_COUNTER.next_serial();
            keyboard.set_focus(self, Some(surface.clone()), serial);
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
