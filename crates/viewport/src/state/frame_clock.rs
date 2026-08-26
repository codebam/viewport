// SPDX-License-Identifier: GPL-3.0-or-later
//
// Event delivery, frame callbacks, frame clocks and conditional rendering.
// Included by `state.rs` to share the state module's imports and privacy.

impl ViewportState {
    /// Send an event to everything listening: the socket clients and the
    /// shell.
    ///
    /// The shell is not a socket client — it is spoken to through JavaScript —
    /// so anything that only broadcasts on the socket is invisible to the one
    /// thing that draws the desktop.
    pub fn notify(&mut self, event: &Event) {
        self.ipc.broadcast(event);
        #[cfg(feature = "wpe")]
        for page in &self.shells {
            // Both directions, because a message that is sent and one that
            // arrives look the same from here and only one of them explains a
            // shell that draws its wallpaper and nothing else.
            tracing::debug!("to shell: {event:?}");
            if let Err(e) = page.engine.post(event) {
                tracing::warn!("could not post to the shell: {e:#}");
            }
        }
    }

    /// Invite the surfaces on an output to draw their next frame.
    ///
    /// Split out of the render pass because a frame callback is not a thing
    /// that happens *because* the compositor drew. It is the compositor
    /// saying "now would be a good time", and a client that paints only when
    /// invited has no other way to hear it.
    pub fn send_frame_callbacks(&mut self, output: &Output, at: std::time::Duration) {
        // Half a frame, not a whole one.
        //
        // Smithay drops an invitation unless more than `throttle` has passed
        // since the last one, strictly greater. Set to the refresh period
        // exactly, that is a knife edge laid on top of a clock that jitters:
        // an invitation arriving a microsecond early is not held back, it is
        // thrown away, and the client waits an entire further frame.
        //
        // What that cost was a constant fraction rather than a constant
        // amount, which is what made it so hard to read as a timing bug. A
        // client that drew on every invitation got 50.3fps of 60, 101.8 of
        // 120, and 203.4 of 239.76 — 85% at every rate, while the compositor
        // sat at 3.6% of a core and flipped on every single vblank. It was
        // never short of time. It was being told to draw five times out of
        // six.
        //
        // Half a period leaves the throttle doing its actual job — an
        // occluded surface still cannot be invited faster than twice a frame —
        // without standing exactly where the jitter falls.
        let throttle = Some(self.frame_interval() / 2);

        // Who was actually waiting to be told. Counted before the send,
        // because the send is what empties the queue. See `FrameLog::wanted`.
        if self
            .udev
            .as_ref()
            .and_then(|udev| udev.frame_log.as_ref())
            .is_some()
        {
            use smithay::wayland::compositor::SurfaceAttributes;
            let mut waiting = 0u32;
            for window in self.space.elements() {
                let mut asked = false;
                window.with_surfaces(|_, states| {
                    let queued = states
                        .cached_state
                        .get::<SurfaceAttributes>()
                        .current()
                        .frame_callbacks
                        .len();
                    asked |= queued > 0;
                });
                if asked {
                    waiting += 1;
                }
            }
            if let Some(log) = self.udev.as_mut().and_then(|udev| udev.frame_log.as_mut()) {
                log.wanted += waiting;
            }
        }
        for window in self.space.elements() {
            window.send_frame(output, at, throttle, |_, _| Some(output.clone()));
        }
        for layer in smithay::desktop::layer_map_for_output(output).layers() {
            layer.send_frame(output, at, throttle, |_, _| Some(output.clone()));
        }
        for lock in self.lock_surfaces.values() {
            smithay::desktop::utils::send_frames_surface_tree(
                lock.wl_surface(),
                output,
                at,
                throttle,
                |_, _| Some(output.clone()),
            );
        }
        // The out-of-process shell. It is not in the space and not in a layer
        // map, so nothing above reaches it — and a client that paints only when
        // invited and is never invited is a desktop that draws one frame and
        // stops.
        //
        // Every page, not only the desktop: a `--url` page on the first monitor
        // is as much a client waiting to be told to draw as the desktop on the
        // second.
        for surface in self.shell_client_surfaces() {
            smithay::desktop::utils::send_frames_surface_tree(
                &surface,
                output,
                at,
                throttle,
                |_, _| Some(output.clone()),
            );
        }
    }

    /// Keep inviting clients to draw for as long as any of them is drawing.
    ///
    /// Frame callbacks used to go out only at the end of a render pass, which
    /// worked by accident: the compositor rendered thousands of times a second
    /// whether or not anything had changed, so every client was invited
    /// constantly. Once renders were held to actual damage that engine went
    /// away, and with it every invitation — a client waiting on a callback to
    /// paint never painted, so it never made damage, so no render happened and
    /// no callback went out. The desktop froze solid and came back only on
    /// input, which forced a frame by another route.
    ///
    /// So the invitations get their own clock. It ticks at the refresh rate
    /// while clients are committing and stops when they stop, which is the
    /// difference between a frame clock and the busy loop it replaces.
    pub fn arm_frame_clock(&mut self) {
        // Recorded even when the clock is already running, so that the tick
        // this request lands behind is not the last one. See `frame_pending`.
        self.frame_pending = true;
        if self.frame_clock {
            return;
        }
        let interval = self.frame_interval();

        if self.frame_timer.is_none() {
            self.frame_timer = self.create_tick("frame clock", Self::frame_tick);
        }
        if Self::arm_tick("frame clock", self.frame_timer.as_ref(), interval) {
            self.frame_clock = true;
            self.frame_clock_at = Some(std::time::Instant::now() + interval);
            return;
        }

        // No timerfd. calloop's own timer still works whenever calloop is the
        // one waiting, which is every backend except the web engine's, so it
        // is worth having rather than dropping the tick entirely.
        self.frame_clock = true;
        self.frame_clock_at = Some(std::time::Instant::now() + interval);
        let timer = smithay::reexports::calloop::timer::Timer::from_duration(interval);
        if let Err(e) = self.loop_handle.insert_source(timer, move |_, _, state| {
            state.frame_tick();
            smithay::reexports::calloop::timer::TimeoutAction::Drop
        }) {
            tracing::warn!("frame clock: {e}");
            self.frame_clock = false;
            self.frame_clock_at = None;
        }
    }

    /// Create a timerfd, put it in the loop, and say what a tick does.
    ///
    /// Returns a second handle on the same timer: the source owns the fd it
    /// watches, and arming happens from outside the source.
    ///
    /// A plain `fn` rather than a closure so that the tick body stays a named
    /// method — these run a frame apart from everything else and are easier to
    /// find when they are not anonymous.
    pub(crate) fn create_tick(
        &mut self,
        what: &'static str,
        run: fn(&mut Self),
    ) -> Option<std::os::fd::OwnedFd> {
        use smithay::reexports::rustix::time::{timerfd_create, TimerfdClockId, TimerfdFlags};

        let fd = match timerfd_create(
            TimerfdClockId::Monotonic,
            TimerfdFlags::NONBLOCK | TimerfdFlags::CLOEXEC,
        ) {
            Ok(fd) => fd,
            Err(e) => {
                tracing::warn!("{what}: no timerfd ({e}), falling back to a loop timer");
                return None;
            }
        };
        let watched = match fd.try_clone() {
            Ok(watched) => watched,
            Err(e) => {
                tracing::warn!("{what}: could not dup the timer ({e})");
                return None;
            }
        };

        if let Err(e) = self.loop_handle.insert_source(
            Generic::new(watched, Interest::READ, Mode::Level),
            move |_, fd, state: &mut Self| {
                // Drained, or a level-triggered source reports the same
                // expiry for ever and the loop never sleeps again.
                let mut buf = [0u8; 8];
                let _ = smithay::reexports::rustix::io::read(&*fd, &mut buf[..]);
                run(state);
                Ok(PostAction::Continue)
            },
        ) {
            tracing::warn!("{what}: could not watch the timer ({e})");
            return None;
        }

        Some(fd)
    }

    /// Set a one-shot timerfd `interval` from now. False if there was none to
    /// set, or the kernel refused it.
    ///
    /// One shot rather than repeating: a repeating timer would keep waking a
    /// desktop that has settled, which is the cost these clocks exist to
    /// avoid. Each tick arms the next itself for as long as there is a reason.
    pub(crate) fn arm_tick(
        what: &'static str,
        fd: Option<&std::os::fd::OwnedFd>,
        interval: std::time::Duration,
    ) -> bool {
        use smithay::reexports::rustix::time::{
            timerfd_settime, Itimerspec, TimerfdTimerFlags, Timespec,
        };

        let Some(fd) = fd else {
            return false;
        };
        let spec = Itimerspec {
            it_interval: Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            it_value: Timespec {
                tv_sec: interval.as_secs() as _,
                tv_nsec: interval.subsec_nanos() as _,
            },
        };
        match timerfd_settime(fd, TimerfdTimerFlags::empty(), &spec) {
            Ok(_) => true,
            Err(e) => {
                tracing::warn!("{what}: could not arm the timer: {e}");
                false
            }
        }
    }

    /// One turn of the frame clock: invite, draw, send.
    fn frame_tick(&mut self) {
        self.frame_clock = false;
        self.frame_clock_at = None;
        let asked = std::mem::take(&mut self.frame_pending);

        let at = self.start_time.elapsed();
        let outputs: Vec<Output> = self.space.outputs().cloned().collect();

        // Only when no vblank is doing it.
        //
        // `on_vblank` invites the clients on an output the moment its frame
        // reaches the screen, which is the right moment and the right rate.
        // This clock exists for when that is not happening at all — nested,
        // headless, or a desktop so still that nothing has been submitted and
        // so no vblank is coming. Sending from both is not redundancy: the
        // client is asked twice per frame period and paints twice, and the
        // compositor shows one of the two. A 60Hz screen measured a client at
        // 120fps with half of its work discarded before anyone saw it.
        //
        // Two frame periods of slack, so this takes over promptly when the
        // chain really has stopped without racing it when it has not.
        let vblank_driven = self
            .udev
            .as_ref()
            .and_then(|udev| udev.last_vblank)
            .is_some_and(|at| at.elapsed() < self.frame_interval() * 2);
        if !vblank_driven {
            for output in &outputs {
                self.send_frame_callbacks(output, at);
            }
        }
        // Counted before the render, because this is the moment the clock is
        // about to do a vblank's job: every render driven from here is a
        // flip-vblank-flip chain that had stopped and is being restarted, and
        // this clock is slower than the screen. See `FrameLog`.
        if let Some(log) = self.udev.as_mut().and_then(|udev| udev.frame_log.as_mut()) {
            log.restarts += 1;
        }
        // One render for everything that happened since the last tick.
        self.render_if_needed();
        let _ = self.display_handle.flush_clients();

        // Anything still owed a frame keeps the clock going; an empty desk
        // lets it stop. `asked` is what covers the surface whose invitation
        // this tick was too early to send — see `frame_pending`. Cleared
        // straight after, because the arming below *is* that follow-up and
        // treating it as a fresh request would leave the clock running for
        // ever.
        if asked || self.needs_render || !self.dirty_outputs.is_empty() {
            self.arm_frame_clock();
            self.frame_pending = false;
        }
    }

    /// Draw any output that has something new to show.
    ///
    /// Called from the outer loop rather than from wherever the change
    /// happened, so a commit that touches five subsurfaces costs one frame
    /// instead of five.
    pub fn render_if_needed(&mut self) {
        // Before the frame is composed from it: the renderer draws in the
        // space's order, so a stack still owed a `restack` here would be a
        // float drawn behind the window it belongs in front of, for as long as
        // that frame is on the screen.
        self.settle();

        let all = std::mem::take(&mut self.needs_render);
        let some = std::mem::take(&mut self.dirty_outputs);
        if !all && some.is_empty() {
            return;
        }
        // Drawing while the screens are off would queue a frame, and a queued
        // frame is what turns them back on.
        if self.udev.as_ref().map(|udev| udev.blanked).unwrap_or(false) {
            return;
        }
        // Nested has no crtcs; that backend redraws continuously and takes
        // what it needs from the same shared frame description.
        let crtcs: Vec<_> = self
            .udev
            .as_ref()
            .map(|udev| {
                udev.ids()
                    .into_iter()
                    .filter(|id| all || some.contains(id))
                    .collect()
            })
            .unwrap_or_default();
        for crtc in crtcs {
            self.render(crtc);
        }
    }

    /// Send every toplevel the configure it has pending.
    ///
    /// Cheap to call on all of them: `send_pending_configure` is a no-op for a
    /// window whose pending state matches what it was last told.
    pub(crate) fn send_pending_configures(&self) {
        for window in self.space.elements() {
            if let Some(toplevel) = window.toplevel() {
                toplevel.send_pending_configure();
            }
        }
    }
}
