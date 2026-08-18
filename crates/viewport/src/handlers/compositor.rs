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

            // An explicit acquire point is not waited for here.
            //
            // Smithay carries it to the two places that actually touch the
            // buffer: `import_surface` waits on it through the renderer, which
            // makes it a wait on the GPU rather than on this thread, and the
            // DRM compositor hands it to KMS as an IN_FENCE_FD so the display
            // waits instead. Neither costs a descriptor in the event loop and
            // neither wakes the compositor.
            //
            // Waiting for it here did both. A blocker is an eventfd, two
            // epoll_ctl to put it in the loop and take it out, a close, and a
            // wakeup when it fires — about eight syscalls per commit per
            // surface, and the second of exactly two wakeups every commit
            // cost. A client in IMMEDIATE mode commits thirteen thousand times
            // a second and paid all of it. Skipping the wait outright, which
            // is wrong and was done only to price it, took that scenario from
            // 52.6% of a core to 31.8%.
            //
            // Asking `is_signaled` first and skipping the blocker when the
            // point had already fired was the cheap version of this and does
            // nothing: 55.1%, still two turns per commit. A client committing
            // that fast is ahead of the GPU by definition, so its acquire
            // point has essentially never signalled by the time the commit
            // arrives.
            if acquire.is_some() {
                return;
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
        // Timed as a whole, including `on_commit_buffer_handler`, because the
        // question this answers is what one commit costs — not what one part
        // of it costs. Two clock reads per commit, and only when the counters
        // are on. See `FrameLog::commit_nanos`.
        let started = self
            .udev
            .as_ref()
            .and_then(|udev| udev.frame_log.as_ref())
            .map(|_| std::time::Instant::now());

        self.commit_inner(surface);

        if let Some(started) = started {
            let spent = started.elapsed().as_nanos() as u64;
            if let Some(log) = self.udev.as_mut().and_then(|udev| udev.frame_log.as_mut()) {
                log.commits += 1;
                log.commit_nanos += spent;
            }
        }
    }
}

impl ViewportState {
    /// The body of [`CompositorHandler::commit`], split out only so the whole
    /// of it can be timed from one place.
    fn commit_inner(&mut self, surface: &WlSurface) {
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

        // The wallpaper terminal, before the window path: it is not a window,
        // and every path below would either ignore it or half-adopt it.
        if self.background_commit(surface) {
            return;
        }

        // Before everything else that looks at a commit: the shell's buffer is
        // taken here, and none of the rest applies to it — it is not a window,
        // not a layer surface and not a lock screen.
        if self.shell_client_commit(surface) {
            // Both clocks, as for any other client: the shell is invited to
            // draw by the same frame callbacks, and it is as entitled to a
            // fifo barrier as anything else that paints.
            self.arm_frame_clock();
            self.arm_barrier_tick();
            return;
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
        // Measurement only: a render that finds nothing to draw after this has
        // lost something a client just painted. See `empty_after_commit`.
        if let Some(udev) = self.udev.as_mut() {
            udev.committed_since_flip = true;
            // The first since the last flip starts the clock; a second one
            // before anything drew has been waiting since the first.
            udev.first_commit_at
                .get_or_insert_with(std::time::Instant::now);
        }
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
        // Before the mutable borrow below, because working out whose dialog
        // this is means looking at the other views.
        let parent = self
            .views
            .find_by_surface(surface)
            .and_then(|view| self.views.parent_id_of(view));
        let Some(view) = self.views.find_by_surface_mut(surface) else {
            return;
        };
        view.mapped = true;
        // What the client asked for before it had anywhere to be told about.
        // Read here rather than trusted from the earlier request, because an
        // X11 window never made a request at all — it carries the state as a
        // property and nothing calls into the compositor about it.
        let fullscreen = view.wants_fullscreen();
        let added = view.added(output, false, parent);

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
    /// Remember to drop whatever a renderer made from this buffer.
    ///
    /// The Vulkan renderer holds one image per shm `wl_buffer` it has uploaded
    /// so that a client painting every frame is not reallocated every frame,
    /// and those entries are keyed by an object id that says nothing when the
    /// object dies. Nothing else would ever clear them — see
    /// `ViewportState::dead_buffers` for what that cost while a screen share
    /// was keeping every client painting.
    ///
    /// Queued rather than done here: this arrives on the client's dispatch,
    /// and the renderer to tell can be moved out of the state at that moment.
    fn buffer_destroyed(&mut self, buffer: &wl_buffer::WlBuffer) {
        self.dead_buffers.push(buffer.clone());
    }
}

impl ShmHandler for ViewportState {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}
