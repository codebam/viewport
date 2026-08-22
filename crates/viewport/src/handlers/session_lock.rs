// SPDX-License-Identifier: GPL-3.0-or-later
//
// ext-session-lock-v1. Ports src/session_lock.c.
//
// A locker is not a window and not a layer surface. It is one surface per
// output, drawn over everything, and while it is up nothing else may be seen
// or typed into — that is the protocol's whole guarantee, and it is the
// compositor's to keep rather than the shell's. The shell is a web page; a
// lock screen that a page could paint over would not be a lock screen.
//
// The other half of the guarantee is what happens when the locker dies. The
// session stays locked: the surfaces go, the screen shows nothing, and the
// only way out is another locker taking over. Unlocking on a crash would make
// killing the locker the way past it.

use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::utils::SERIAL_COUNTER;
use smithay::wayland::session_lock::{
    LockSurface, SessionLockHandler, SessionLockManagerState, SessionLocker,
};

use crate::state::ViewportState;

impl SessionLockHandler for ViewportState {
    fn lock_state(&mut self) -> &mut SessionLockManagerState {
        &mut self.session_lock_state
    }

    fn lock(&mut self, confirmation: SessionLocker) {
        // A second locker over a working one is refused.
        //
        // Smithay grants every `lock` request — it builds a fresh lock object
        // with its own status flag and calls this, on the assumption that the
        // compositor decides — so nothing below this refuses it for us. Taking
        // it clears `lock_surfaces` a few lines down, which drops the surfaces
        // of the locker that is *still running and still drawing*: two clients
        // then own one screen, only the newer one is rendered, and unlocking
        // the one you can see leaves the other holding a lock nothing can
        // reach. That is a session that stays locked after a correct password,
        // showing the clear colour and no way back.
        if self.locked && self.lock_screen_is_drawing() {
            tracing::warn!(
                "a locker asked for a session that another locker is already \
                 drawing; refusing it. Dropping the request tells that client \
                 `finished`, which is what it is waiting to hear."
            );
            // Dropped rather than confirmed: `SessionLocker::drop` sends
            // `finished`, and swaylock exits on it.
            return;
        }

        tracing::info!("session locked");
        self.locked = true;
        self.locked_at = Some(std::time::Instant::now());
        self.lock_warned = false;
        self.lock_surfaces.clear();
        // Whatever the shell was drawing, it is not the lock screen any more.
        //
        // Reached where the built-in screen is up and not drawing — a shell
        // that hung or died — which is the case a locker is *meant* to be able
        // to take over, and the refusal above already turned away the case
        // where it is drawing. Dropping it here stops the shell's next frame
        // from being drawn over the locker that has just taken the session,
        // and tells the page to put its own lock screen away.
        if self.lock_shell_drawn.take().is_some() {
            self.notify(&viewport_ipc::Event::SessionUnlock);
        }
        self.lock_attempt = None;

        // Keyboard focus goes nowhere until a lock surface arrives and takes
        // it. Leaving it where it was would let the previously focused window
        // keep receiving keys behind the lock screen.
        if let Some(keyboard) = self.seat.get_keyboard() {
            let serial = SERIAL_COUNTER.next_serial();
            keyboard.set_focus(self, None, serial);
        }

        // Confirmed only once nothing else can be seen, which is what the
        // client waits for before it will show anything of its own.
        confirmation.lock();
        self.needs_render = true;
    }

    fn unlock(&mut self) {
        tracing::info!("session unlocked");
        self.locked = false;
        self.locked_at = None;
        self.lock_surfaces.clear();
        // The page is told either way. It will not normally have a lock screen
        // up when an external locker unlocks — the two do not run together —
        // but it does after a takeover of a built-in lock that had stopped
        // drawing, and a page left holding one over an unlocked session is a
        // desktop nobody can click.
        if self.lock_shell_drawn.take().is_some() || self.lock_mode.is_built_in() {
            self.notify(&viewport_ipc::Event::SessionUnlock);
        }
        self.lock_attempt = None;
        self.needs_render = true;
    }

    fn new_surface(&mut self, surface: LockSurface, output: WlOutput) {
        use smithay::output::Output;

        let Some(output) = Output::from_resource(&output) else {
            // Nothing to size it against, and a lock surface with no size
            // never paints — which is a locked session showing whatever was
            // on screen before, with no way back.
            tracing::error!("a lock surface arrived for an output we do not know");
            return;
        };
        let Some(geometry) = self.space.output_geometry(&output) else {
            tracing::error!(
                "a lock surface arrived for {} which is not mapped",
                output.name()
            );
            return;
        };
        tracing::info!("lock surface on {}", output.name());

        // The whole output, because that is the only size a lock surface may
        // be. It will not paint until it has been told.
        surface.with_pending_state(|state| {
            state.size = Some((geometry.size.w as u32, geometry.size.h as u32).into());
        });
        surface.send_configure();

        self.lock_surfaces.insert(output.name(), surface);
        self.needs_render = true;
    }
}

impl ViewportState {
    /// Whether a lock screen is up and on at least one screen.
    ///
    /// The question both refusals ask: a locked session whose locker has gone
    /// is one anybody may take over — that is the documented way out of a
    /// crashed locker, and `check_lock_screen` tells the user to do exactly
    /// that. A locked session that is *drawing* is not, and a second locker
    /// over it is how one ends up unreachable.
    ///
    /// It is also the fail-closed gate for the lock screen this compositor
    /// draws itself, and that is the harder half. The renderer asks this
    /// before it will put a single pixel of the shell's buffer on a locked
    /// screen, and false here is a black screen — which is an acceptable
    /// failure, where the desktop showing through is not.
    ///
    /// For the built-in screen it takes two facts, and neither alone is
    /// enough:
    ///
    /// * The shell has said, naming this lock, that it has painted the lock
    ///   screen. A message alone is not proof of a pixel: a page can send one
    ///   from a handler and then never paint. But a live process is not the
    ///   test either — a page that is running and stuck is exactly the case —
    ///   so something the page has to say is the only way to know it got as
    ///   far as building the thing.
    ///
    /// * *And* a frame has landed since it said so. The page sends `drawn`
    ///   from a double `requestAnimationFrame`, which runs strictly after the
    ///   frame the lock screen was rendered into was submitted — so any buffer
    ///   arriving after that message is that frame or a later one, and every
    ///   one of them has the lock screen in it. The frame the page committed
    ///   *before* it was told to lock is the desktop, and drawing that is
    ///   exactly the failure this guard exists to stop: a locked session
    ///   showing the bar, the window titles, and whatever the notification
    ///   centre last had in it.
    ///
    /// Both facts are dropped on anything that could invalidate them — a new
    /// lock, the shell process dying, its toplevel going away — so the answer
    /// after any of those is false until the page has drawn and said so again.
    /// A hung shell never says so, and a dead one cannot; both are black.
    ///
    /// The message is not privileged. Anything that can reach the control
    /// socket can send it, and the generation it names is broadcast rather
    /// than secret. That is deliberate and not a hole worth closing here: the
    /// socket is reachable only by processes already running as this user,
    /// which is a position from which the desktop was readable before the lock
    /// was ever taken. What this guards against is the shell being *broken*,
    /// which is the failure that actually happens.
    pub fn lock_screen_is_drawing(&self) -> bool {
        use smithay::utils::IsAlive as _;
        if self
            .lock_surfaces
            .values()
            .any(|surface| surface.wl_surface().alive())
        {
            return true;
        }
        self.lock_mode.is_built_in()
            && self.lock_shell_drawn.is_some_and(|(lock, frames)| {
                lock == self.lock_generation && self.shell_frames > frames
            })
    }

    /// Forget that the shell had drawn the lock screen.
    ///
    /// Called wherever the page that drew it stops being the page on screen:
    /// its process died, its toplevel went away, it was reloaded. The next
    /// thing rendered on a locked session is then black until the page that
    /// replaced it has drawn a lock screen of its own and said so.
    ///
    /// Separate from the lock itself on purpose. The session stays locked
    /// across every one of those — a shell that crashes must not be a way past
    /// the lock — and only what is *drawn* is withdrawn.
    pub fn forget_lock_screen(&mut self) {
        if self.lock_shell_drawn.take().is_some() {
            tracing::info!(
                "lock: the page that drew the lock screen has gone. The session \
                 stays locked and the screen goes black until something draws \
                 one again."
            );
            self.needs_render = true;
        }
    }

    /// Say so if the session is locked and nothing is drawing a lock screen.
    ///
    /// A locker that exits after taking the lock leaves the session locked
    /// with no way to authenticate — correct by the protocol, and identical to
    /// what the C build does, but from the front it is a black screen that
    /// eats every key. The only ways out are another locker or a VT switch,
    /// and neither is guessable from a screen that says nothing.
    ///
    /// Warned once per lock rather than every tick.
    pub fn check_lock_screen(&mut self) {
        if !self.locked || self.lock_warned {
            return;
        }
        let Some(at) = self.locked_at else {
            return;
        };

        // A locker that drew and then exited leaves surfaces behind that no
        // longer exist. Keeping them means rendering a dead client and never
        // noticing that nothing is on screen any more.
        use smithay::utils::IsAlive as _;
        let before = self.lock_surfaces.len();
        self.lock_surfaces
            .retain(|_, surface| surface.wl_surface().alive());
        if self.lock_surfaces.len() != before {
            self.needs_render = true;
        }

        if at.elapsed() < std::time::Duration::from_secs(3) {
            return;
        }

        // The built-in screen has no per-output surfaces to count — it is one
        // buffer across the whole layout — so what stands in for "nothing has
        // drawn" is the same gate the renderer asks before it draws any of it.
        //
        // The advice differs too, and that is the point of saying it
        // separately: an external locker that never draws is a program that
        // crashed, and the answer is to run another one. A shell that never
        // draws is the desktop itself, and the answer is `idle.lock_command` —
        // a locker that is not this shell, for a machine whose shell will not
        // paint.
        if self.lock_mode.is_built_in() {
            if self.lock_screen_is_drawing() {
                return;
            }
            self.lock_warned = true;
            tracing::error!(
                "locked, but the shell has not drawn a lock screen. The session \
                 stays locked — a shell that crashes must not be a way past the \
                 lock — so the way out is Ctrl+Alt+F1..F12 to another VT. Set \
                 idle.lock_command if this machine should lock with a locker of \
                 its own instead."
            );
            return;
        }

        let missing: Vec<String> = self
            .space
            .outputs()
            .map(|output| output.name())
            .filter(|name| !self.lock_surfaces.contains_key(name))
            .collect();
        if missing.is_empty() {
            return;
        }
        self.lock_warned = true;
        tracing::error!(
            "locked, but nothing has drawn a lock screen on {}. \
             The locker has probably exited. The session stays locked — that is \
             what the protocol asks for — so the way out is Ctrl+Alt+F1..F12 to \
             another VT, or running another locker against this display.",
            missing.join(", ")
        );
    }

    /// Focus a lock surface, so the locker can be typed into.
    ///
    /// Called when one commits: focusing at `new_surface` would be too early,
    /// since the client has not acknowledged its size and has nothing to show.
    pub fn focus_lock_surface(
        &mut self,
        surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    ) {
        if !self.locked {
            return;
        }
        let ours = self
            .lock_surfaces
            .values()
            .any(|lock| lock.wl_surface() == surface);
        if !ours {
            return;
        }
        let already = self
            .seat
            .get_keyboard()
            .and_then(|keyboard| keyboard.current_focus())
            .map(|focused| focused.is_surface(surface))
            .unwrap_or(false);
        if already {
            return;
        }
        if let Some(keyboard) = self.seat.get_keyboard() {
            let serial = SERIAL_COUNTER.next_serial();
            keyboard.set_focus(self, Some(surface.clone().into()), serial);
        }
        self.needs_render = true;
    }
}
