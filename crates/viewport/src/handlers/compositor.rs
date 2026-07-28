// SPDX-License-Identifier: GPL-3.0-or-later

use smithay::backend::renderer::utils::{on_commit_buffer_handler, with_renderer_surface_state};
use smithay::reexports::wayland_server::protocol::{wl_buffer, wl_surface::WlSurface};
use smithay::reexports::wayland_server::Client;
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    get_parent, is_sync_subsurface, CompositorClientState, CompositorHandler, CompositorState,
};
use smithay::wayland::shm::{ShmHandler, ShmState};

use viewport_ipc::Event;

use crate::state::{ClientState, ViewportState};

use super::xdg_shell;

impl CompositorHandler for ViewportState {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        // Xwayland's own connection is inserted by Smithay with its own data
        // type, not this compositor's. Assuming otherwise aborts the whole
        // session the moment Xwayland connects, which is at startup.
        if let Some(state) = client.get_data::<smithay::xwayland::XWaylandClientData>() {
            return &state.compositor_state;
        }
        if let Some(state) = client.get_data::<ClientState>() {
            return &state.compositor_state;
        }
        panic!("a client with neither this compositor's data nor Xwayland's")
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);

        if !is_sync_subsurface(surface) {
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            if let Some(view) = self.views.find_by_surface(&root) {
                view.window.on_commit();
            }
        }

        xdg_shell::handle_commit(self, surface);
        // A layer surface has no size until it is arranged, and will not paint
        // until it has been configured.
        self.layer_commit(surface);
        self.focus_layer_if_exclusive(surface);

        self.announce_if_newly_mapped(surface);

        // A client painted. Rendering is driven by vblank and vblank stops
        // when nothing is submitted, so with a still screen there is nothing
        // to carry this to an output — the window would update only when
        // something unrelated caused a frame.
        self.needs_render = true;
    }
}

impl ViewportState {
    /// Tell the shell about a window the moment its client first paints.
    ///
    /// Announcing at `new_toplevel` would be too early: the window has no
    /// title, no app_id and no size yet, and the shell would place an empty
    /// rectangle and then have to be told all three again.
    fn announce_if_newly_mapped(&mut self, surface: &WlSurface) {
        let Some(view) = self.views.find_by_surface(surface) else {
            return;
        };
        if view.mapped {
            return;
        }

        let has_buffer =
            with_renderer_surface_state(surface, |state| state.buffer().is_some()).unwrap_or(false);
        if !has_buffer {
            return;
        }

        let output = self.output_for_new_view();
        let Some(view) = self.views.find_by_surface_mut(surface) else {
            return;
        };
        view.mapped = true;
        let added = view.added(output, false);

        // What the client actually handed over. Whether a window is opaque is
        // a property of its buffer format, and guessing at that is what the
        // last two attempts at "the window is transparent" did.
        let kind = with_renderer_surface_state(surface, |state| {
            let Some(buffer) = state.buffer() else {
                return "no buffer".to_owned();
            };
            if let Ok(dmabuf) = smithay::wayland::dmabuf::get_dmabuf(buffer) {
                use smithay::backend::allocator::Buffer as _;
                let format = dmabuf.format();
                return format!("dmabuf {:?} modifier {:?}", format.code, format.modifier);
            }
            match smithay::wayland::shm::with_buffer_contents(buffer, |_, _, data| data.format) {
                Ok(format) => format!("shm {format:?}"),
                Err(e) => format!("neither dmabuf nor shm: {e:?}"),
            }
        })
        .unwrap_or_else(|| "no surface state".to_owned());
        tracing::info!("view {}: {kind}", added.id);

        self.notify(&Event::ViewAdded(added));
    }
}

impl BufferHandler for ViewportState {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for ViewportState {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}
