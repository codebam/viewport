// SPDX-License-Identifier: GPL-3.0-or-later
//
// misc:enable_anr_dialog — a window that stops responding.
//
// The compositor pings each mapped xdg window on a timer. Smithay tracks the
// pending ping per client and checks the pong's serial, and hands the pong
// back as `XdgShellHandler::client_pong`; a window whose pong is overdue is
// flagged and the shell asked to show a wait-or-force-quit dialog. The force
// quit is Smithay's own `ShellClient::unresponsive`, which posts the
// `xdg_wm_base` unresponsive error and disconnects the client — the protocol's
// answer to a window that will not answer, rather than a signal sent behind
// its back.
//
// Only xdg windows are pinged: the ping/pong liveness contract is
// `xdg_wm_base`'s. An X11 window has no such ping, so it is left alone.

use std::time::{Duration, Instant};

use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::utils::SERIAL_COUNTER;
use smithay::wayland::shell::xdg::ShellClient;

use crate::state::ViewportState;
use viewport_ipc::Event;

/// How often to ping, and how long a pong may take before a window is called
/// unresponsive. A second between pings is gentle enough not to wake a desk
/// that is idle; five seconds is long enough that a busy client — a compile,
/// a load — is not mistaken for a dead one.
const PING_INTERVAL: Duration = Duration::from_secs(1);
const ANR_TIMEOUT: Duration = Duration::from_secs(5);

impl ViewportState {
    /// Arm the ping timer once, at startup. It ticks whether or not the
    /// dialog is enabled; `anr_tick` is a no-op when it is off, so turning the
    /// policy on later needs no second timer.
    pub fn arm_anr_timer(&mut self) {
        if let Err(e) = self.loop_handle.insert_source(
            Timer::from_duration(PING_INTERVAL),
            move |_, _, state| {
                state.anr_tick();
                TimeoutAction::ToDuration(PING_INTERVAL)
            },
        ) {
            tracing::warn!("could not arm the ANR ping timer: {e}");
        }
    }

    /// One ping round: ping the windows not already waiting on a pong, and
    /// flag the ones whose pong is overdue.
    pub fn anr_tick(&mut self) {
        if !self.anr_enabled {
            return;
        }
        let now = Instant::now();
        // Collected first: pinging borrows the surface and flagging borrows
        // the state mutably, so the two cannot overlap.
        let candidates: Vec<u32> = self
            .views
            .iter()
            .filter(|v| v.mapped && v.visible && !v.minimized)
            .filter(|v| v.window.toplevel().is_some())
            .map(|v| v.id)
            .collect();

        for id in candidates {
            if self.anr_flagged.contains(&id) {
                continue;
            }
            if let Some(sent) = self.anr_pending.get(&id).copied() {
                if now.duration_since(sent) >= ANR_TIMEOUT {
                    self.anr_pending.remove(&id);
                    self.anr_flagged.insert(id);
                    self.notify_anr(id);
                }
                continue;
            }
            let Some(view) = self.views.get(id) else {
                continue;
            };
            let Some(toplevel) = view.window.toplevel() else {
                continue;
            };
            let serial = SERIAL_COUNTER.next_serial();
            if toplevel.client().send_ping(serial).is_ok() {
                self.anr_pending.insert(id, now);
            }
        }
    }

    /// A client answered its ping. Clear its windows' waiting pongs, and take
    /// down the dialog of any that had been flagged: it is responding again.
    pub fn anr_pong(&mut self, client: &ShellClient) {
        let ids: Vec<u32> = self
            .views
            .iter()
            .filter(|v| v.window.toplevel().is_some_and(|t| &t.client() == client))
            .map(|v| v.id)
            .collect();
        for id in ids {
            self.anr_pending.remove(&id);
            if self.anr_flagged.remove(&id) {
                self.notify(&Event::ViewAnrResolved { id });
            }
        }
    }

    /// The shell's answer to the dialog: `"quit"` forces the client closed,
    /// `"wait"` leaves it be. Either way the flag is cleared, so a window that
    /// stays quiet is asked about again on a later round rather than once.
    pub fn anr_resolve(&mut self, id: u32, action: &str) {
        self.anr_flagged.remove(&id);
        self.anr_pending.remove(&id);
        if action == "quit" {
            if let Some(toplevel) = self.views.get(id).and_then(|v| v.window.toplevel()) {
                if let Err(e) = toplevel.client().unresponsive() {
                    tracing::debug!("ANR force quit of view {id}: {e:?}");
                }
            }
        }
    }

    /// Tell the shell a window has stopped responding, so it can ask whether
    /// to wait or force quit.
    fn notify_anr(&mut self, id: u32) {
        let Some(view) = self.views.get(id) else {
            return;
        };
        tracing::info!("view {id} is not responding to ping");
        self.notify(&Event::ViewAnr {
            id,
            app_id: view.app_id(),
            title: view.title(),
        });
    }
}
