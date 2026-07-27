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
        &client.get_data::<ClientState>().unwrap().compositor_state
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

        self.announce_if_newly_mapped(surface);
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

        self.ipc.broadcast(&Event::ViewAdded(added));
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
