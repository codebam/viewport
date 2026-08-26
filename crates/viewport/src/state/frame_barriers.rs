// SPDX-License-Identifier: GPL-3.0-or-later
//
// Frame barriers, shell overlays, damage and frame intervals.
// Included by `state.rs` to share the state module's imports and privacy.

impl ViewportState {
    /// Let go of everything a client is waiting on for this frame.
    ///
    /// Two protocols block a commit until the compositor says so: wp-fifo,
    /// where a client asks to be paced by the display, and wp-commit-timing,
    /// where it asks for a commit to land at a particular time. Both are the
    /// compositor's to release, and a client whose barrier is never signalled
    /// does not simply lose the feature — it never paints again.
    ///
    /// Called where the frame callbacks are sent, which is the moment the
    /// frame this surface is part of has been handed to the display.
    pub fn release_frame_barriers(&mut self, output: &Output, frame_target: std::time::Duration) {
        let _ = self.released_frame_barriers(output, frame_target);
    }

    /// The same, reporting whether anything was actually let go.
    ///
    /// The tick needs to know: a round that releases nothing is a round that
    /// did not need to happen, and enough of those in a row means the clock
    /// can stop.
    pub fn released_frame_barriers(
        &mut self,
        output: &Output,
        frame_target: std::time::Duration,
    ) -> bool {
        self.released_barriers(output, frame_target, false)
    }

    /// Let go of commit-timing deadlines that are already past, and nothing
    /// else.
    ///
    /// For a screen this round is otherwise skipping because its own vblank is
    /// doing the releasing. A fifo barrier there is that vblank's to take —
    /// taking it here is what paced clients off this timer instead of off the
    /// screen. A deadline that has *already passed* is different: it is not
    /// pacing anything, it is only holding a commit.
    ///
    /// It holds it because Smithay blocks every commit carrying a deadline
    /// whether or not the deadline has arrived — unlike its fifo hook, which
    /// skips a barrier that is already signalled. So a commit aimed at a
    /// moment that has been and gone waits for whatever runs next, and on a
    /// screen with no frame coming that is this and only this.
    pub fn release_overdue_timers(&mut self, output: &Output) -> bool {
        let at = self.start_time.elapsed();
        self.released_barriers(output, at, true)
    }

    fn released_barriers(
        &mut self,
        output: &Output,
        frame_target: std::time::Duration,
        overdue_only: bool,
    ) -> bool {
        use smithay::desktop::utils::with_surfaces_surface_tree;
        use smithay::utils::Time;
        use smithay::wayland::commit_timing::CommitTimerBarrierStateUserData;
        use smithay::wayland::fifo::FifoBarrierCachedState;

        // The clock the *client* set its deadline on, which is CLOCK_MONOTONIC
        // and not this compositor's uptime. `frame_target` is time since the
        // compositor started — smaller than the real clock by however long the
        // machine has been up — so using it means every deadline is in the
        // future for ever and every timed commit blocks until the client gives
        // up. That is a client frozen on its first frame, and it looks exactly
        // like the fifo barrier not being signalled.
        let _ = frame_target;
        let now = smithay::reexports::rustix::time::clock_gettime(
            smithay::reexports::rustix::time::ClockId::Monotonic,
        );
        // The deadline to compare against is the frame this round is about to
        // draw, which is the *next* refresh — not this instant.
        //
        // A commit-timing deadline says "do not show this before T". The frame
        // being built now is the one that will be presented at the next
        // vblank, so what belongs in it is everything due by then. Comparing
        // against the present moment instead holds a commit aimed at the next
        // vblank until that vblank has already happened, so it misses the
        // frame it was aimed at and goes in the one after.
        //
        // Mesa aims exactly there — one refresh ahead — on every frame, so
        // every frame was arriving a frame late, and any jitter in when this
        // round ran turned "late" into "a whole refresh late". That is the
        // client sitting at five sixths of the rate: with commit-timing off
        // and fifo left on, the same client goes from 204.1fps to 239.2 of a
        // possible 239.76.
        //
        // Except when only the overdue are wanted: then the moment is now, so
        // a deadline aimed at the frame after this one is left for the vblank
        // that will actually show it.
        let refresh = if overdue_only {
            std::time::Duration::ZERO
        } else {
            self.frame_interval()
        };
        let target: Time<smithay::utils::Monotonic> =
            (std::time::Duration::new(now.tv_sec as u64, now.tv_nsec as u32) + refresh).into();
        let released = std::cell::Cell::new(false);
        // Counted rather than just flagged: `released` answers "was this round
        // worth running", and what the pacing question needs is how many.
        let signalled = std::cell::Cell::new(0u32);
        let woken: std::cell::RefCell<Vec<smithay::reexports::wayland_server::Client>> =
            std::cell::RefCell::new(Vec::new());

        let release = |surface: &WlSurface, states: &smithay::wayland::compositor::SurfaceData| {
            let wake = |signalled: bool| {
                if !signalled {
                    return;
                }
                if let Some(client) = surface.client() {
                    let mut woken = woken.borrow_mut();
                    if !woken.iter().any(|c| c.id() == client.id()) {
                        woken.push(client);
                    }
                }
            };
            if let Some(mut timer) = states
                .data_map
                .get::<CommitTimerBarrierStateUserData>()
                // Read, not obeyed: this runs every barrier round, and a lock
                // poisoned once by some other thread's panic would pace no
                // client ever again.
                .map(|timer| timer.lock().unwrap_or_else(|e| e.into_inner()))
            {
                if timer.signal_until(target) {
                    tracing::trace!("commit-timing: a deadline reached");
                    released.set(true);
                    wake(true);
                }
            }
            // The current half, taken out. This is what `wayland::fifo`
            // documents and what anvil does, and both halves of that matter.
            //
            // Taken, because a barrier left in place is found again on the next
            // round, already signalled, and reported as nothing released —
            // which is what `QUIET` counts, and enough of those stop the clock
            // under a client that is still waiting.
            //
            // Current only, because pending is where the pre-commit hook looks
            // for the barrier to hand the *next* commit, and it skips blocking
            // outright if what it finds is already signalled
            // (`wayland/fifo/mod.rs:257`). Signalling pending is therefore not
            // a belt-and-braces release: it is switching the pacing off. The
            // barrier a blocked commit is waiting on is not lost by leaving
            // pending alone — Smithay carries it in the transaction and puts it
            // in the current half when that commit applies, which is the round
            // this signals it in.
            // Left alone entirely when only the overdue are wanted: this
            // screen has a vblank coming, and that vblank is what a fifo
            // barrier means. Taking it here would pace the client off this
            // timer rather than off the screen, which is the drift measured at
            // five sixths of the refresh rate.
            let barrier = if overdue_only {
                None
            } else {
                states
                    .cached_state
                    .get::<FifoBarrierCachedState>()
                    .current()
                    .barrier
                    .take()
            };
            if let Some(barrier) = barrier {
                barrier.signal();
                tracing::trace!("fifo: a barrier released");
                released.set(true);
                signalled.set(signalled.get() + 1);
                wake(true);
            }
        };

        // Only the windows on this output. Walking every window once per
        // output did the same work twice on a two-monitor desktop and made a
        // release on one screen look like a reason to draw the other.
        for window in self.space.elements_for_output(output) {
            window.with_surfaces(&release);
        }
        for layer in smithay::desktop::layer_map_for_output(output).layers() {
            layer.with_surfaces(&release);
        }
        for lock in self.lock_surfaces.values() {
            with_surfaces_surface_tree(lock.wl_surface(), &release);
        }
        for surface in self.shell_client_surfaces() {
            with_surfaces_surface_tree(&surface, &release);
        }
        // And the wallpaper terminal, which is in none of the four collections
        // above and is as entitled to be paced as anything else that paints.
        //
        // Leaving it out is a client that paints its swapchain full and then
        // stops for ever. rio does exactly that: mesa's Vulkan WSI paces on
        // wp-fifo, three buffers went out in the first thirty milliseconds,
        // the fourth commit blocked on a barrier nothing here ever signalled,
        // and what was on screen was a terminal's first blank frame. It looked
        // precisely like the wallpaper not being drawn at all — which is what
        // it was reported as — and foot hid it, because foot paints into
        // shared memory and asks for no pacing.
        for surface in self.background_surfaces() {
            with_surfaces_surface_tree(&surface, &release);
        }
        // After the walks, so the closure's borrows are done with.
        let signalled = signalled.get();
        if signalled > 0 {
            if let Some(log) = self.udev.as_mut().and_then(|udev| udev.frame_log.as_mut()) {
                log.barriers += signalled;
            }
        }
        // The part that makes any of it work. Signalling a barrier only sets a
        // flag; the commit it was blocking sits in a queue that nothing looks
        // at again until the compositor says a blocker cleared. Without this
        // the client commits for ever and the compositor applies none of them,
        // which from the outside is a window frozen on its first frame while
        // the client is busy and healthy.
        let woken = woken.into_inner();
        if !woken.is_empty() {
            let dh = self.display_handle.clone();
            for client in woken {
                if let Some(data) = client.get_data::<crate::state::ClientState>() {
                    data.compositor_state.blocker_cleared(self, &dh);
                }
            }
        }
        released.get()
    }

    /// Whether anything is waiting on a barrier, as far as can be told.
    ///
    /// A fifo barrier sits in the surface's current state from the commit that
    /// set it until the compositor takes it, so it can be seen directly. A
    /// commit timer cannot: Smithay keeps its deadlines private and offers no
    /// way to ask whether any are left. So a surface that has ever used one
    /// counts as waiting, and `arm_barrier_tick` stops re-arming after a
    /// stretch of ticks that release nothing — the next commit starts the
    /// clock again, which is the only moment a new deadline can appear.
    pub fn barriers_outstanding(&self) -> bool {
        use smithay::wayland::commit_timing::CommitTimerBarrierStateUserData;
        use smithay::wayland::fifo::{FifoBarrierCachedState, FifoCachedState};

        // Once any surface has ever been seen holding this state, the answer
        // is yes without looking: the requests that set it are answered inside
        // Smithay, so this walk is the only thing that could notice one
        // arriving, and there is no moment where it leaving is visible either.
        // A monotonic flag rather than a count, for exactly that reason — the
        // cost is a tick that keeps running on a desktop whose fifo client is
        // long gone, against a walk over every window's tree on every commit,
        // which is what this exists to stop paying. See `barrier_ever_armed`.
        if self.barrier_ever_armed.get() {
            return true;
        }

        let mut waiting = false;
        {
            let mut look =
                |_surface: &WlSurface, states: &smithay::wayland::compositor::SurfaceData| {
                    if waiting {
                        return;
                    }
                    // Not "is a barrier sitting here" — that misses the case
                    // this whole tick exists for. A commit blocked on a barrier
                    // has had that barrier taken out of the surface state by
                    // the pre-commit hook and handed to the blocker, so the
                    // surface looks empty at exactly the moment the client is
                    // stuck. What it does not hide is that the client asked
                    // for fifo at all, which is in `FifoCachedState`.
                    //
                    // So: a surface that uses either protocol keeps the clock
                    // running. A fifo client wants a frame every refresh
                    // anyway, and the frame is what carries the callback and
                    // the presentation feedback it is waiting on.
                    let mut fifo_request = states.cached_state.get::<FifoCachedState>();
                    let asks_for_fifo = {
                        let pending = *fifo_request.pending();
                        let current = *fifo_request.current();
                        pending.set_barrier
                            || pending.wait_barrier
                            || current.set_barrier
                            || current.wait_barrier
                    };
                    let mut fifo = states.cached_state.get::<FifoBarrierCachedState>();
                    if asks_for_fifo
                        || fifo.current().barrier.is_some()
                        || fifo.pending().barrier.is_some()
                        || states
                            .data_map
                            .get::<CommitTimerBarrierStateUserData>()
                            .is_some()
                    {
                        waiting = true;
                    }
                };
            for window in self.space.elements() {
                window.with_surfaces(&mut look);
            }
            // And the wallpaper terminal, which is not in the space.
            //
            // Without it the clock stops under a blocked wallpaper: nothing
            // else on an otherwise empty desktop is waiting, so the tick
            // decides there is nothing to keep running for and the one client
            // that needed the next round never gets it.
            for surface in self.background_surfaces() {
                smithay::desktop::utils::with_surfaces_surface_tree(&surface, &mut look);
            }
        }
        if waiting {
            self.barrier_ever_armed.set(true);
        }
        waiting
    }

    /// Keep a clock running while a client is blocked on a barrier.
    ///
    /// This is the half that was missing when these two protocols were first
    /// advertised and then withdrawn. The compositor draws when there is
    /// damage; a blocked commit produces none, so the frame that would have
    /// released the barrier never happens and the client waits for a
    /// compositor that is waiting for the client. Six hundred lines of
    /// "nothing to draw" and a terminal frozen on its first frame.
    ///
    /// So while anything is outstanding, a timer runs at roughly the refresh
    /// interval, signals what is due, and asks for a frame. Once nothing is
    /// waiting the timer stops, and an idle desktop goes back to drawing
    /// nothing at all.
    pub fn arm_barrier_tick(&mut self) {
        if self.barrier_tick {
            return;
        }
        if !self.barriers_outstanding() {
            return;
        }
        let interval = self.frame_interval();

        // On a timerfd, for the same reason the frame clock is: this tick is
        // the *only* thing that can free a client blocked on a barrier when
        // nothing else is happening. A blocked commit makes no damage, so
        // there is no frame, so there is no vblank, so `on_vblank` — the other
        // half of the release — never runs either. Under the web engine GLib
        // owns the blocking poll and cannot see a calloop timer, so this tick
        // used to arrive only when a mouse or another window woke the loop for
        // unrelated reasons. That is a terminal on an empty workspace showing
        // nothing of what is typed into it: rio paints through Mesa, Mesa
        // paces itself with `wp_fifo_v1`, and every one of its commits waits
        // on a barrier this tick was supposed to lift.
        if self.barrier_timer.is_none() {
            self.barrier_timer = self.create_tick("barrier tick", Self::release_barriers);
        }
        if Self::arm_tick("barrier tick", self.barrier_timer.as_ref(), interval) {
            self.barrier_tick = true;
            return;
        }

        self.barrier_tick = true;
        let timer = smithay::reexports::calloop::timer::Timer::from_duration(interval);
        if let Err(e) = self.loop_handle.insert_source(timer, move |_, _, state| {
            state.release_barriers();
            smithay::reexports::calloop::timer::TimeoutAction::Drop
        }) {
            tracing::warn!("arming the barrier tick: {e}");
            self.barrier_tick = false;
        }
    }

    /// One turn of the barrier tick: let go of whatever is due.
    fn release_barriers(&mut self) {
        self.barrier_tick = false;
        if let Some(log) = self.udev.as_mut().and_then(|udev| udev.frame_log.as_mut()) {
            log.barrier_ticks += 1;
        }

        // Not while the vblank is doing it.
        //
        // A fifo barrier says "the frame this commit made has been shown", so
        // the moment to signal it is the vblank that showed it. This tick is
        // for a desk where no frame is being submitted and so no vblank is
        // coming — a client blocked on a barrier makes no damage, which makes
        // no frame, which makes no vblank to lift it.
        //
        // Left running alongside the vblank it does not add safety, it takes
        // the job over. It fires part way through the frame period and takes
        // the barrier before the frame it belongs to has been presented, so
        // the vblank arrives to an empty queue and does nothing — measured at
        // 60Hz: 50 barriers a second, every one of them released here and none
        // at the vblank, with ten vblanks a second finding nothing to do.
        //
        // The client is then paced by this timer rather than by the screen,
        // and this timer is armed after its own work, so it is always slower.
        // That is the whole of a client sitting at five sixths of the refresh
        // rate at every rate tried — 50.4 of 60, 101.8 of 120, 203.4 of 240 —
        // while the compositor flipped on every single vblank at under 2% of
        // a core.
        //
        // Still re-armed below, so it takes over within two frames if the
        // chain really does stop.
        //
        // Asked once per screen rather than once for the device. It used to be
        // one stamp — "has *anything* flipped lately" — and the release it
        // defers to is one screen's windows, so a second monitor animating at
        // the refresh rate kept that stamp fresh and silenced this tick for a
        // screen it never visited. Measured on a two-screen desk: 238 turns a
        // second, every one of them deferred, and every one of those deferrals
        // made on behalf of a screen that had not flipped.
        //
        // What waits behind it is worse than a late fifo barrier. Smithay
        // blocks *every* commit carrying a commit-timing deadline, including
        // one already in the past — unlike its fifo hook, which skips a
        // barrier that is already signalled — so this pass is the only thing
        // that lets such a commit through. Deferring it on another screen's
        // behalf is how a terminal ends up at seven frames a second on a 240Hz
        // display with the compositor idle.
        let interval = self.frame_interval();
        let at = self.start_time.elapsed();
        let outputs: Vec<Output> = self.space.outputs().cloned().collect();
        let mut released = false;
        let mut walked = 0usize;
        for output in &outputs {
            // This screen's own last flip, not the newest anywhere. A screen
            // whose vblank is doing the releasing does not need this pass.
            let own_vblank = self.udev.as_ref().is_some_and(|udev| {
                udev.last_vblank_by_output
                    .get(&output.name())
                    .is_some_and(|at| at.elapsed() < interval * 2)
            });
            if own_vblank {
                // Its vblank has the fifo barriers. What that vblank will not
                // do is let go of a deadline that has already passed, because
                // it only signals up to the frame it is about to show — and a
                // commit held on a stale deadline is not waiting for a frame,
                // it is just waiting.
                if self.release_overdue_timers(output) {
                    released = true;
                    self.mark_output_dirty(output);
                }
                continue;
            }
            walked += 1;
            // Counted before the call, so what it adds to `barriers` can be
            // told apart afterwards: this is the tick's share of the releases,
            // and under a client painting flat out it should be nearly none.
            let before = self
                .udev
                .as_ref()
                .and_then(|udev| udev.frame_log.as_ref())
                .map(|log| log.barriers);
            if self.released_frame_barriers(output, at) {
                released = true;
                self.mark_output_dirty(output);
            }
            if let Some(before) = before {
                if let Some(log) = self.udev.as_mut().and_then(|udev| udev.frame_log.as_mut()) {
                    log.barriers_at_tick += log.barriers.saturating_sub(before);
                }
            }
        }
        // Every screen was flipping on its own, so this turn had nothing to
        // do. The same early exit as before, reached per-screen rather than
        // for the device — and `starved` should now stay at zero, because a
        // screen that has not flipped is one this walked.
        if walked == 0 {
            let starved = self.udev.as_ref().is_some_and(|udev| {
                self.space.outputs().any(|output| {
                    udev.last_vblank_by_output
                        .get(&output.name())
                        .is_none_or(|at| at.elapsed() >= interval * 2)
                })
            });
            if let Some(log) = self.udev.as_mut().and_then(|udev| udev.frame_log.as_mut()) {
                log.barrier_ticks_deferred += 1;
                if starved {
                    log.barrier_ticks_starved += 1;
                }
            }
            self.arm_barrier_tick();
            return;
        }
        // Only when something was let go. Releasing a barrier applies the
        // commit it was blocking, and an applied commit is damage, and damage
        // is what asks for a frame — so the frame arrives without being
        // demanded here. Asking anyway drew every output at the refresh rate
        // for as long as one client used the protocol, which on a second
        // monitor with nothing on it is pure heat.
        if released {
            self.barrier_quiet = 0;
        } else {
            self.barrier_quiet = self.barrier_quiet.saturating_add(1);
        }
        // A tick that has released nothing for a second is a tick nobody
        // needs: the deadlines that could not be seen have all passed, or
        // there were none. A commit is the only thing that can make a new one,
        // and a commit arms this again.
        //
        // Unless something is still waiting. `QUIET` is a backstop for
        // commit-timing, whose deadlines Smithay keeps private so an empty
        // round cannot be told from a finished one — but fifo *can* be seen,
        // and a fifo client's blocked commit never reaches `commit()` to arm
        // this again. Letting the count stop the clock under one is how a
        // terminal ends up waiting on a compositor that has stopped looking.
        if self.barrier_quiet < Self::QUIET || self.barriers_outstanding() {
            self.arm_barrier_tick();
        }
    }

    /// Replace the list of shell rectangles that float above the windows.
    ///
    /// Ids are kept by position and only minted when the list grows, because a
    /// render element with a new id every frame tells the damage tracker that
    /// everything changed. When the list shrinks the ids beyond it go too:
    /// kept, they would grow by whatever the largest list ever sent was, for
    /// the life of the session.
    ///
    /// `hits` is the subset of them that takes the pointer. Everything the
    /// shell floats does, bar one: see `shell_overlay_hits`.
    pub fn set_shell_overlays(
        &mut self,
        rects: Vec<smithay::utils::Rectangle<i32, Logical>>,
        hits: Vec<smithay::utils::Rectangle<i32, Logical>>,
    ) {
        // A cap rather than trust: the list comes over the control socket, and
        // the render elements it becomes are walked on every frame. A client
        // that sends millions is refused here rather than allowed to grow the
        // element list until the desktop cannot draw.
        if rects.len() > MAX_SHELL_OVERLAYS {
            tracing::warn!(
                "shell.overlay sent {} rectangles; more than the {} allowed, refused",
                rects.len(),
                MAX_SHELL_OVERLAYS
            );
            self.notify(&Event::Error {
                context: "shell.overlay".to_owned(),
                message: format!(
                    "{} rectangles is more than the {} this compositor takes",
                    rects.len(),
                    MAX_SHELL_OVERLAYS
                ),
            });
            return;
        }
        if self.shell_overlays == rects && self.shell_overlay_hits == hits {
            return;
        }
        self.shell_overlay_hits = hits;
        // What is under the pointer just changed without the pointer moving,
        // and the pointer's focus is only worked out when it moves.
        //
        // This is what made notifications unclickable. One appears over a
        // window, under a pointer that is sitting still; the compositor knows
        // the click belongs to the shell — `surface_under` checks the overlays
        // first — but the *pointer* still has the window underneath as its
        // focus, and a button event goes to the focus rather than to a fresh
        // hit test. The click landed on whatever the notification was covering.
        //
        // The in-process backend never showed this: there, a click over an
        // overlay was routed by re-running the hit test at button time, since
        // the shell was not a surface and could not be a pointer focus. Moving
        // the shell into a client is what turned "checked on every click" into
        // "checked on every motion", and this is the half that went missing.
        self.refresh_pointer_focus();
        while self.shell_overlay_ids.len() < rects.len() {
            self.shell_overlay_ids
                .push(smithay::backend::renderer::element::Id::new());
        }
        // And the other half of "kept by position": ids past the end of the
        // list they belong to are not kept by anything. Shrunk rather than
        // drained-and-reminted, so a list that oscillates in length does not
        // churn new ids — and new full-frame damage — every time it dips.
        self.shell_overlay_ids.truncate(rects.len());
        self.shell_overlays = rects;
        // The stack changed without anything committing, and a desktop nobody
        // is touching produces no damage of its own — so without this the
        // notification appears on the next frame something else happens to
        // cause, which on an idle desktop is none.
        self.needs_render = true;
    }

    /// Ask for a frame on whichever screens show this surface.
    ///
    /// A client painting at its own rate is the common case, and marking the
    /// whole desktop for it means the other monitor attempts a frame per
    /// commit and finds nothing — two thousand of them in five seconds, for a
    /// cube on the first screen. Falls back to everything when the surface is
    /// not a window this compositor has placed: a layer surface, a popup, the
    /// lock screen, or a window between mapping and being given a rectangle.
    pub fn mark_dirty_for_surface(&mut self, surface: &WlSurface) {
        let mut root = surface.clone();
        while let Some(parent) = smithay::wayland::compositor::get_parent(&root) {
            root = parent;
        }
        let outputs = self
            .views
            .find_by_surface(&root)
            .map(|view| self.space.outputs_for_element(&view.window))
            .unwrap_or_default();
        if outputs.is_empty() {
            self.needs_render = true;
            return;
        }
        for output in outputs {
            self.mark_output_dirty(&output);
        }
    }

    /// Ask for a frame on one output rather than all of them.
    ///
    /// A pacing barrier belongs to a window, a window is on a screen, and the
    /// other screen has no reason to be redrawn for it. Falls back to marking
    /// everything if the output has no CRTC here, which is the nested backend
    /// and the moment between a monitor arriving and being brought up.
    pub fn mark_output_dirty(&mut self, output: &Output) {
        self.arm_frame_clock();
        let crtc = self.udev.as_ref().and_then(|udev| {
            udev.outputs()
                .find(|(_, surface)| &surface.output == output)
                .map(|(id, _)| id)
        });
        match crtc {
            Some(crtc) => {
                self.dirty_outputs.insert(crtc);
            }
            None => self.needs_render = true,
        }
    }

    /// How long one frame lasts on the fastest output, near enough.
    ///
    /// Near enough because this paces a fallback clock rather than the display
    /// itself: a barrier released a millisecond late is a frame late at worst,
    /// and the alternative is no frame ever.
    pub fn frame_interval(&self) -> std::time::Duration {
        self.space
            .outputs()
            .filter_map(|output| output.current_mode())
            .map(|mode| mode.refresh.max(1) as u64)
            .max()
            .map(|refresh| std::time::Duration::from_nanos(1_000_000_000_000 / refresh))
            .unwrap_or_else(|| std::time::Duration::from_millis(16))
    }
}
