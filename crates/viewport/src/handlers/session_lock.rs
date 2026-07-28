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
        tracing::info!("session locked");
        self.locked = true;
        self.lock_surfaces.clear();

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
        self.lock_surfaces.clear();
        self.needs_render = true;
    }

    fn new_surface(&mut self, surface: LockSurface, output: WlOutput) {
        use smithay::output::Output;

        let Some(output) = Output::from_resource(&output) else {
            return;
        };
        let Some(geometry) = self.space.output_geometry(&output) else {
            return;
        };

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
    /// Focus a lock surface, so the locker can be typed into.
    ///
    /// Called when one commits: focusing at `new_surface` would be too early,
    /// since the client has not acknowledged its size and has nothing to show.
    pub fn focus_lock_surface(&mut self, surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface) {
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
            .map(|focused| &focused == surface)
            .unwrap_or(false);
        if already {
            return;
        }
        if let Some(keyboard) = self.seat.get_keyboard() {
            let serial = SERIAL_COUNTER.next_serial();
            keyboard.set_focus(self, Some(surface.clone()), serial);
        }
        self.needs_render = true;
    }
}
