// SPDX-License-Identifier: GPL-3.0-or-later
//
// Power snapshots, lid policy, locking, unlocking and blanking.
// Included by `state.rs` to share the state module's imports and privacy.

impl ViewportState {
    /// A snapshot from the UPower worker: paint the bar, and act on the lid
    /// if it moved.
    pub fn handle_power(&mut self, snapshot: viewport_ipc::event::PowerSnapshot) {
        let closed = snapshot.lid_closed;
        if self.last_lid_closed != Some(closed) {
            self.last_lid_closed = Some(closed);
            if self.lid != crate::power::LidAction::Ignore {
                if closed {
                    self.apply_lid_close();
                } else {
                    self.set_outputs_enabled(true);
                }
            }
        }
        if self.power.widget() {
            self.notify(&viewport_ipc::Event::PowerUpdate(snapshot));
        }
    }

    /// The lid just closed.
    pub fn apply_lid_close(&mut self) {
        match self.lid {
            crate::power::LidAction::Ignore => {}
            crate::power::LidAction::Lock => self.lock_session(),
            crate::power::LidAction::Blank => self.blank_screens(),
            crate::power::LidAction::Suspend => self.power.suspend(),
        }
    }

    /// Lock the session, however this machine is configured to.
    ///
    /// The one answer to what locking means, and every way of asking for it
    /// comes through here: the idle deadline, the `lock` binding, the lid
    /// action, and the power menu's Lock row over `session.lock`. That was
    /// already the property this function was written for
    /// (`src/binding.c:614`); it matters more now that there are two things it
    /// can mean. Which one is `self.lock_mode`, worked out once per config
    /// load — see `crate::lock::Mode`.
    pub fn lock_session(&mut self) {
        // Not over a locker that is already up.
        //
        // The lock handler refuses the second lock, but by then a whole second
        // locker has been started, has authenticated nobody, and is waiting to
        // be told `finished`. Cheaper and quieter to not run it: the idle
        // deadline fires on a session somebody locked by hand five minutes
        // earlier, which is how two swaylocks ended up on one screen.
        //
        // A locked session with no locker drawing is *not* this, and is left
        // alone deliberately — running another locker against it is the way
        // out of a locker that crashed, and `check_lock_screen` says so.
        if self.locked && self.lock_screen_is_drawing() {
            tracing::info!("lock: a locker is already drawing; leaving it alone");
            return;
        }

        match self.lock_mode.clone() {
            crate::lock::Mode::Command(command) => {
                let display = self.child_display_env();
                crate::input::spawn_with_env(&command, &display)
            }
            // No locker configured, which used to be a warning and nothing
            // else: `lock` bound to a chord did nothing, and the lid and the
            // idle deadline did nothing either. Now it is the shell's own lock
            // screen. See `crate::lock` for why that is the better default and
            // what a desk with no keyboard could not do before it.
            crate::lock::Mode::BuiltIn => self.lock_with_shell(),
        }
    }

    /// Lock the session and ask the shell to draw the lock screen.
    ///
    /// The compositor takes the lock itself here rather than waiting for a
    /// client to ask for one, which is the whole difference between this and
    /// the ext-session-lock path: there is no second process to crash, so
    /// there is nothing to wait for. Everything the protocol handler does on a
    /// `lock` request is done here for the same reasons, and the comments
    /// there are the argument for each of them.
    ///
    /// What the page then has to do is in `Event::SessionLock`. What happens
    /// if it does not is the point of `lock_screen_is_drawing`: the session is
    /// locked from this line onwards whatever the shell does next, and nothing
    /// of the shell's buffer reaches the screen until it has said it has drawn
    /// *and* painted a frame after saying so.
    fn lock_with_shell(&mut self) {
        // A new lock is a new generation, always — including a re-lock of a
        // session that is already locked with a shell that has stopped
        // drawing. The old generation's `drawn` must not carry over, because
        // the page that sent it is the page that stopped.
        self.lock_generation = self.lock_generation.wrapping_add(1);
        self.lock_shell_drawn = None;
        self.lock_attempt = None;
        self.locked = true;
        self.locked_at = Some(std::time::Instant::now());
        self.lock_warned = false;
        self.lock_surfaces.clear();

        let generation = self.lock_generation;
        let can_authenticate = self.authenticator.online();
        if !can_authenticate {
            // Loud, because from the front this is a password box that will
            // never open and there is nothing on screen that could explain
            // why. Locked anyway: a session that refuses to lock because it
            // cannot check a password is a laptop that goes into a bag with
            // the desktop on screen.
            tracing::error!(
                "lock: no authentication worker, so no password can be checked. \
                 The session is locked all the same — the way out is another VT \
                 (Ctrl+Alt+F1..F12) or an idle.lock_command that runs a locker \
                 of its own."
            );
        }
        tracing::info!("session locked; the shell draws the lock screen (lock {generation})");
        self.notify(&viewport_ipc::Event::SessionLock {
            generation,
            can_authenticate,
        });

        // The keyboard goes to the shell, which is the opposite of what the
        // protocol handler does and is right for the same reason. There, focus
        // is dropped because the locker has not made its surface yet and the
        // window that had the keyboard must not keep it; here the surface that
        // will draw the lock screen already exists, and the password has to go
        // somewhere. Nothing else can reach it: `surface_under` answers with
        // the shell and nothing else while the session is locked, and no
        // binding fires.
        self.focus_lock_shell();
        self.needs_render = true;
    }

    /// Put the keyboard on the shell for a lock it is drawing.
    ///
    /// Called at the lock and again whenever the shell restarts under one, so
    /// a page that crashed and came back is typable without the person having
    /// to find the mouse — which on the desk this feature exists for is not a
    /// thing they have.
    pub fn focus_lock_shell(&mut self) {
        if !self.locked || !self.lock_mode.is_built_in() {
            return;
        }
        if !self.focus_shell_at(None) {
            // No shell client to focus. Either the WPE backend, which is not a
            // client and takes keys another way, or a compositor running with
            // no shell at all — a test. Focus goes nowhere rather than staying
            // on the window that had it.
            if let Some(keyboard) = self.seat.get_keyboard() {
                let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                keyboard.set_focus(self, None, serial);
            }
        }
    }

    /// The shell says it has painted the lock screen.
    ///
    /// Recorded with the frame count at the moment it was said, which is the
    /// half of the rule that a message cannot fake: see
    /// `lock_screen_is_drawing`.
    pub fn lock_screen_drawn(&mut self, generation: u64) {
        if !self.locked || !self.lock_mode.is_built_in() {
            return;
        }
        if generation != self.lock_generation {
            tracing::debug!(
                "lock: the shell says it drew lock {generation}, but this is lock {}",
                self.lock_generation
            );
            return;
        }
        if self.lock_shell_drawn.map(|(lock, _)| lock) == Some(generation) {
            return;
        }
        tracing::info!("lock: the shell has drawn lock {generation}");
        self.lock_shell_drawn = Some((generation, self.shell_frames));
        self.needs_render = true;
    }

    /// Somebody typed a password at the lock screen.
    ///
    /// Handed to the worker thread and answered later; nothing here waits.
    /// Every refusal below answers the page rather than dropping the message,
    /// because a lock screen whose Enter key does nothing is indistinguishable
    /// from one that is broken, and the person's next move is to hold the
    /// power button.
    pub fn try_unlock(&mut self, generation: u64, password: viewport_ipc::request::Secret) {
        if !self.locked || !self.lock_mode.is_built_in() {
            // Nothing to unlock, or a locker of somebody else's is holding it
            // — in which case this compositor has no business checking a
            // password on its behalf, and unlocking on one would be a way
            // past a lock screen it does not own.
            return;
        }
        if generation != self.lock_generation {
            tracing::debug!("lock: a password arrived for lock {generation}, which is over");
            return;
        }
        if self.lock_attempt.is_some() {
            tracing::debug!("lock: an attempt is already with PAM; dropping this one");
            return;
        }
        if !self.authenticator.ask(crate::lock::Attempt {
            generation,
            password,
        }) {
            self.notify(&viewport_ipc::Event::SessionLockError {
                generation,
                message: "this session cannot check a password".to_owned(),
            });
            return;
        }
        self.lock_attempt = Some(generation);
    }

    /// What PAM said about it.
    pub fn handle_lock_verdict(&mut self, verdict: crate::lock::Verdict) {
        if self.lock_attempt == Some(verdict.generation) {
            self.lock_attempt = None;
        }
        if !self.locked || verdict.generation != self.lock_generation {
            // The lock ended while the stack was thinking — a takeover, a
            // `viewport msg`. The verdict is about a lock that is over, and a
            // true one must not unlock the lock that came after it.
            return;
        }
        if verdict.ok {
            tracing::info!("lock: the password was accepted");
            self.unlock_session();
            return;
        }
        let message = verdict
            .message
            .unwrap_or_else(|| "that password was not accepted".to_owned());
        // At info, not warn, and without a user name: a wrong password at a
        // lock screen is the ordinary case — somebody typing with the caps
        // lock on — and a log that shouts about it teaches people to ignore
        // the log.
        tracing::info!("lock: refused — {message}");
        self.notify(&viewport_ipc::Event::SessionLockError {
            generation: verdict.generation,
            message,
        });
    }

    /// Take the built-in lock screen down.
    ///
    /// Only ever called with a verdict behind it. There is no other caller and
    /// deliberately no IPC message that reaches it: an `unlock` on the control
    /// socket would be a lock screen anything on the machine could dismiss.
    fn unlock_session(&mut self) {
        self.locked = false;
        self.locked_at = None;
        self.lock_warned = false;
        self.lock_shell_drawn = None;
        self.lock_attempt = None;
        self.lock_surfaces.clear();
        tracing::info!("session unlocked");
        self.notify(&viewport_ipc::Event::SessionUnlock);
        // Back to whatever the desktop decides, which for an empty desk is the
        // shell and for a desk with windows is the window that had the
        // keyboard before the lock. `focus_shell_if_idle` is the floor under
        // both; a window that wants the keyboard back takes it on the next
        // click, exactly as it would after any other loss of focus.
        self.focus_shell_if_idle();
        self.needs_render = true;
    }

    /// Turn the screens off now.
    ///
    /// Flagged as though the deadline had done it, so the next input brings
    /// them back through the same path. Blanking without that leaves no way to
    /// undo it short of a deadline that has already fired.
    pub fn blank_screens(&mut self) {
        self.idle.force_blank();
        self.set_outputs_enabled(false);
    }
}
