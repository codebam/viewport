// SPDX-License-Identifier: GPL-3.0-or-later

use smithay::backend::renderer::utils::{on_commit_buffer_handler, with_renderer_surface_state};
use smithay::reexports::wayland_server::protocol::{wl_buffer, wl_surface::WlSurface};
use smithay::reexports::wayland_server::{Client, Resource as _};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    add_blocker, add_pre_commit_hook, get_parent, is_sync_subsurface, with_states,
    BufferAssignment, CompositorClientState, CompositorHandler, CompositorState, SurfaceAttributes,
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

    /// Hold a commit back until the client's drawing has actually finished.
    ///
    /// A client hands over a buffer its GPU may still be writing into. The
    /// kernel's implicit fences cover that for a GL client, but a Vulkan one
    /// attaches no implicit fence and nvidia's driver has never had them at
    /// all — so the compositor samples a half-drawn frame and the window shows
    /// torn or stale contents for one frame at a time.
    ///
    /// linux-drm-syncobj-v1 replaces the guess: the client names a timeline
    /// point that signals when the buffer is ready. That point becomes a
    /// blocker on the commit, and the commit is applied when it fires. A
    /// client that does not use the protocol falls back to the dmabuf's own
    /// fence, which is the implicit path this never had either.
    fn new_surface(&mut self, surface: &WlSurface) {
        add_pre_commit_hook::<Self, _>(surface, move |state, _dh, surface| {
            let mut acquire = None;
            let dmabuf = with_states(surface, |states| {
                acquire.clone_from(
                    &states
                        .cached_state
                        .get::<smithay::wayland::drm_syncobj::DrmSyncobjCachedState>()
                        .pending()
                        .acquire_point,
                );
                match states
                    .cached_state
                    .get::<SurfaceAttributes>()
                    .pending()
                    .buffer
                    .as_ref()
                {
                    Some(BufferAssignment::NewBuffer(buffer)) => {
                        smithay::wayland::dmabuf::get_dmabuf(buffer).cloned().ok()
                    }
                    _ => None,
                }
            });
            // Shared memory is written by the CPU and is finished by the time
            // the commit arrives; there is nothing to wait for.
            let Some(dmabuf) = dmabuf else {
                return;
            };
            let Some(client) = surface.client() else {
                return;
            };

            // The client's own point first: it knows when it is done, and the
            // buffer's fence may cover work that has nothing to do with this
            // frame.
            if let Some(acquire) = acquire {
                if let Ok((blocker, source)) = acquire.generate_blocker() {
                    let client = client.clone();
                    let inserted = state.loop_handle.insert_source(source, move |_, _, state| {
                        let dh = state.display_handle.clone();
                        state
                            .client_compositor_state(&client)
                            .blocker_cleared(state, &dh);
                        Ok(())
                    });
                    if inserted.is_ok() {
                        add_blocker(surface, blocker);
                        return;
                    }
                }
            }

            // No explicit point, so the buffer's own fence — which is what a
            // GL client relies on and what was missing here entirely.
            if let Ok((blocker, source)) =
                dmabuf.generate_blocker(smithay::reexports::calloop::Interest::READ)
            {
                let inserted = state.loop_handle.insert_source(source, move |_, _, state| {
                    let dh = state.display_handle.clone();
                    state
                        .client_compositor_state(&client)
                        .blocker_cleared(state, &dh);
                    Ok(())
                });
                if inserted.is_ok() {
                    add_blocker(surface, blocker);
                }
            }
        });
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
        self.focus_lock_surface(surface);

        self.announce_if_newly_mapped(surface);
        self.trace_size_mismatch(surface);

        // A client painted. Rendering is driven by vblank and vblank stops
        // when nothing is submitted, so with a still screen there is nothing
        // to carry this to an output — the window would update only when
        // something unrelated caused a frame.
        //
        // Only the screens this surface is actually on: a client painting at
        // 120Hz on one monitor had the other attempting a frame per commit for
        // nothing.
        self.mark_dirty_for_surface(surface);
        // Whether or not it painted, this client is awake, so keep the
        // invitations going. A commit that carried no damage is often exactly
        // a client acknowledging a configure and waiting to be told when to
        // draw.
        self.arm_frame_clock();

        // If this commit set a fifo barrier or a commit timer, the *next* one
        // is going to block, and a blocked commit makes no damage to draw. The
        // clock that releases it starts here rather than at the next frame,
        // because there may not be a next frame.
        self.arm_barrier_tick();
    }
}

impl ViewportState {
    /// Tell the shell about a window the moment its client first paints.
    ///
    /// Announcing at `new_toplevel` would be too early: the window has no
    /// title, no app_id and no size yet, and the shell would place an empty
    /// rectangle and then have to be told all three again.
    /// Say when what a client painted is not the size it was asked for.
    ///
    /// A window drawn at a size other than its rectangle is either scaled or
    /// cropped depending on where it is drawn, and both look like a
    /// compositor bug from the outside. Only on a change, because a client
    /// that ignores a configure would otherwise say so sixty times a second.
    fn trace_size_mismatch(&mut self, surface: &WlSurface) {
        let Some(view) = self.views.find_by_surface(surface) else {
            return;
        };
        let Some(configured) = view.configured else {
            return;
        };
        let id = view.id;
        let scale = view.scale;
        let Some(size) = with_renderer_surface_state(surface, |state| state.surface_size()) else {
            return;
        };
        let Some(size) = size else {
            return;
        };
        if (size.w, size.h) == configured {
            return;
        }
        let Some(view) = self.views.find_by_surface_mut(surface) else {
            return;
        };
        if view.last_mismatch == Some((size.w, size.h)) {
            return;
        }
        view.last_mismatch = Some((size.w, size.h));
        tracing::debug!(
            "view {id}: painted {}x{} for a rectangle of {}x{} (scale {scale})",
            size.w,
            size.h,
            configured.0,
            configured.1
        );
    }

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
        // What the client asked for before it had anywhere to be told about.
        // Read here rather than trusted from the earlier request, because an
        // X11 window never made a request at all — it carries the state as a
        // property and nothing calls into the compositor about it.
        let fullscreen = view.wants_fullscreen();
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

        let id = added.id;
        let (title, app_id) = (added.title.clone(), added.app_id.clone());
        self.notify(&Event::ViewAdded(added));

        // After the announcement, never before: this is the same command the
        // runtime path sends, and it only means anything once the shell has a
        // window to apply it to.
        if fullscreen {
            self.notify_fullscreen(id, true);
        }

        // Announce it outside the compositor too, now that it has a title and
        // an app id — before that there is nothing worth listing.
        let handle = self
            .foreign_toplevel_state
            .new_toplevel::<Self>(&title, &app_id);
        if let Some(view) = self.views.get_mut(id) {
            view.foreign = Some(handle);
        }
        // And on the older protocol, which is the one a taskbar can act
        // through. Both describe the same windows: a client that binds one of
        // them must not see a different desktop from a client that binds the
        // other.
        let dh = self.display_handle.clone();
        self.foreign_management_state
            .add::<Self>(&dh, id, &title, &app_id);

        // Watch for the shell answering. A window that maps and is never given
        // a rectangle is invisible for ever, and a shell that has stopped
        // answering gives no other sign — the session looks like a black
        // screen with a working keyboard.
        let timer =
            smithay::reexports::calloop::timer::Timer::from_duration(crate::watchdog::TIMEOUT);
        if let Err(e) = self.loop_handle.insert_source(timer, move |_, _, state| {
            state.watchdog_fire(id);
            smithay::reexports::calloop::timer::TimeoutAction::Drop
        }) {
            tracing::warn!("could not arm the layout watchdog for view {id}: {e}");
        }
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
