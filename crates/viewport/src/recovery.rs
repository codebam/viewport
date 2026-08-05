// SPDX-License-Identifier: GPL-3.0-or-later
//
// A GPU that crashes and comes back.
//
// An overclocked card that pushes too far does not fail politely. The kernel
// notices a job that never finished, resets the engine — amdgpu calls it a mode1
// reset, i915 an engine reset — and brings the device back a second or two
// later. Sometimes the reset does not take and the driver falls back to a bus
// reset, which unregisters the DRM device and registers it again: from
// userspace that is the card being unplugged and plugged back in.
//
// Neither of those is a reason to lose the session, and until this existed both
// of them did. What broke was quieter than a crash:
//
//   * A page flip is queued and the reset eats it. The vblank that would have
//     said "that frame is on screen" never arrives, `Surface::pending` stays
//     true, and `render` returns early on it for ever. The screen freezes with
//     the compositor still running, still taking input, still answering the
//     control socket. Nothing in the log says anything is wrong.
//   * The commit fails outright. `queue_frame` logged a warning and gave up —
//     "a failure here stops the output for good rather than dropping one frame"
//     is what the comment in `render_pass` says, and it was accurate.
//   * The Vulkan device is lost. Every submission from that `VkDevice` fails
//     from then on, permanently, and the only cure is building a new one. The
//     renderer had no way to be rebuilt.
//   * The card is unregistered. The vblank source is watching a revoked fd,
//     which polls ready for ever — a compositor spinning on a core over a GPU
//     that is not there. `UdevEvent::Removed` was explicitly ignored.
//
// So a GPU is watched rather than trusted. A flip that does not come back
// within [`STALL`] starts a ladder of increasingly expensive recoveries, each
// tried once before the next: resume the chain, reset the device, rebuild the
// renderer, reopen the card. Every step short of the last is cheap enough to
// run on a false alarm, and the last one is what a hot-unplug needs anyway.
//
// The ladder resets the moment a vblank arrives, so a card that recovers at
// step one never reaches step two, and a card that keeps resetting is retried
// with a backoff rather than in a loop.

use std::path::Path;
use std::time::Duration;

use smithay::backend::drm::{DrmNode, NodeType};
use smithay::reexports::drm::control::crtc;

use crate::state::ViewportState;
use crate::udev::{Device, OutputId};

/// How long a queued flip may take before the output is presumed stalled.
///
/// Generously long. A 60Hz screen flips every 16ms and the slowest display this
/// compositor drives is nowhere near a second, so anything past this is not
/// slow, it is stopped. Erring long costs a frozen screen a little longer;
/// erring short means recovering a display that was about to flip anyway, and
/// the cheapest step of the ladder still resets its buffers.
pub const STALL: Duration = Duration::from_millis(1200);

/// How often the watchdog looks, while there is anything to look at.
///
/// Not a frame's worth. This tick is the fallback for the case where the frame
/// clock and the vblank chain have both stopped, and waking three times a
/// second to check a counter is not something a still desktop will notice.
pub const TICK: Duration = Duration::from_millis(400);

/// The longest wait between two attempts to reopen a card that will not come
/// back. A GPU that is genuinely gone should not be retried forever at speed;
/// one that is coming back after a bus reset takes seconds, not minutes.
pub const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Where a device is in the recovery ladder.
///
/// Each step is tried once. If the next watchdog tick still finds the device
/// stalled, the step after it is tried — so a transient lost vblank costs a
/// buffer reset, and a card that has genuinely gone works its way down to being
/// reopened without anyone having to decide in advance which it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Step {
    /// Nothing is wrong: this is where a device sits, and where a vblank puts
    /// it back.
    #[default]
    Healthy,
    /// Clear the stuck flip, throw away the damage history, draw again.
    ///
    /// The whole fix when the reset only ate the event: the device is fine, the
    /// compositor is merely waiting for something that is not coming.
    Resume,
    /// Take DRM master again and put the mode back.
    ///
    /// What a VT switch back does, for the same reason: after a reset nothing
    /// in the compositor's idea of the device's state is known to be true.
    Reactivate,
    /// Build a new renderer on the same card.
    ///
    /// For `VK_ERROR_DEVICE_LOST`, which is permanent for that `VkDevice`. The
    /// GBM device outlives it, which is why `Device::gbm` is kept.
    Rebuild,
    /// Close the card and open it again.
    ///
    /// The last resort, and the only thing that helps when the DRM device was
    /// unregistered and registered again. Repeats with a backoff rather than
    /// giving up: a card behind a bus reset comes back on its own schedule.
    Reopen,
}

impl Step {
    /// The next thing to try.
    pub fn next(self) -> Step {
        match self {
            Step::Healthy => Step::Resume,
            Step::Resume => Step::Reactivate,
            Step::Reactivate => Step::Rebuild,
            Step::Rebuild => Step::Reopen,
            // The bottom of the ladder repeats. See the variant.
            Step::Reopen => Step::Reopen,
        }
    }

    /// What to put in the log when this step is taken.
    pub fn describe(self) -> &'static str {
        match self {
            Step::Healthy => "nothing to do",
            Step::Resume => "resuming the flip chain",
            Step::Reactivate => "resetting the device",
            Step::Rebuild => "rebuilding the renderer",
            Step::Reopen => "reopening the card",
        }
    }
}

/// How long to wait before trying to reopen a card again.
///
/// The first few attempts are quick, because a bus reset takes a second or two
/// and being ready when it finishes is the difference between a screen that
/// blinks and one that stays dark. After that it doubles, up to [`BACKOFF_MAX`]
/// — a machine whose GPU has been pulled should not spend the rest of the
/// session opening `/dev/dri/card1` three times a second.
pub fn backoff(attempts: u32) -> Duration {
    let doubled = Duration::from_millis(500).saturating_mul(1u32 << attempts.min(6));
    doubled.min(BACKOFF_MAX)
}

/// The bus address behind a `/sys/dev/char/M:m/device` symlink.
///
/// The last component of what it points at: `../../../0000:03:00.0` is the PCI
/// address of the card, and it is the one name for a GPU that survives the card
/// being unregistered and registered again.
///
/// `/dev/dri/card1` does not survive it. A card that comes back while anything
/// still holds an fd on the old one takes the next free minor, so the device
/// that went as `card1` returns as `card2` — matching a re-added GPU by its node
/// path means never recognising the one that just came back.
pub fn bus_id_of(link: &Path) -> Option<String> {
    let name = link.file_name()?.to_string_lossy().into_owned();
    (!name.is_empty()).then_some(name)
}

/// The bus address of a card, read from sysfs.
pub fn bus_id(node: &DrmNode) -> Option<String> {
    let link = std::fs::read_link(format!(
        "/sys/dev/char/{}:{}/device",
        node.major(),
        node.minor()
    ))
    .ok()?;
    bus_id_of(&link)
}

/// Whether a device with these flips outstanding has stopped.
///
/// Pure so it can be tested: the watchdog's whole job is deciding this, and
/// deciding it wrongly is either a frozen screen nobody rescues or a working
/// display being reset under a client.
pub fn stalled(oldest_queued: Option<Duration>, errors: u32) -> bool {
    // An error on the last commit is not waited out. Nothing was queued, so no
    // vblank is coming, and the age of a flip that was never made is not a
    // number that will ever grow.
    if errors > 0 {
        return true;
    }
    oldest_queued.is_some_and(|age| age >= STALL)
}

/// How long a recovery step is given to work before the next one is tried.
///
/// A GPU reset is not instant. amdgpu takes somewhere between one and five
/// seconds to bring an engine back, and during that window every commit fails —
/// so a ladder that escalated on every tick would be four steps deep and
/// reopening the card before the driver had finished resetting it. That still
/// recovers, eventually, but it reopens a card that was about to answer and
/// counts the failure against the retry backoff, which makes the next attempt
/// slower for no reason.
pub const DWELL: Duration = Duration::from_millis(1500);

/// Whether the step now in progress has had its chance.
///
/// `None` means no step is in progress, which is where a healthy device sits —
/// the first escalation off `Healthy` is immediate, because by then the flip has
/// already been outstanding for [`STALL`].
pub fn may_escalate(since: Option<Duration>, wait: Duration) -> bool {
    since.is_none_or(|elapsed| elapsed >= wait)
}

/// How long to leave a step in place before acting again.
///
/// [`DWELL`] for the rungs that are tried once and move on. The bottom rung is
/// not tried once — it repeats, because a card behind a bus reset comes back on
/// its own schedule — so it takes the retry backoff instead, and a card that
/// will not open stops being asked several times a second.
pub fn step_wait(step: Step, attempts: u32) -> Duration {
    match step {
        Step::Reopen => backoff(attempts),
        _ => DWELL,
    }
}

impl ViewportState {
    /// Watch the GPUs until nothing is outstanding.
    ///
    /// Armed from wherever a flip is queued or an error is noticed, and re-armed
    /// by its own tick for as long as there is a reason. A timerfd rather than a
    /// calloop timer, for the reason `arm_frame_clock` gives: under the web
    /// engine GLib owns the blocking poll and calloop's own timer wheel is
    /// invisible to it, so a wheel entry would fire whenever something else
    /// happened to wake the loop — which on a frozen desktop is never, and a
    /// frozen desktop is precisely the case this exists for.
    pub fn watch_gpus(&mut self) {
        if self.gpu_watch {
            return;
        }
        if self.gpu_timer.is_none() {
            self.gpu_timer = self.create_tick("gpu watchdog", Self::gpu_tick);
        }
        if Self::arm_tick("gpu watchdog", self.gpu_timer.as_ref(), TICK) {
            self.gpu_watch = true;
            return;
        }

        // No timerfd. calloop's timer still works whenever calloop is the one
        // waiting, which is every backend but the web engine's.
        self.gpu_watch = true;
        let timer = smithay::reexports::calloop::timer::Timer::from_duration(TICK);
        if let Err(e) = self.loop_handle.insert_source(timer, move |_, _, state| {
            state.gpu_tick();
            smithay::reexports::calloop::timer::TimeoutAction::Drop
        }) {
            tracing::warn!("gpu watchdog: {e}");
            self.gpu_watch = false;
        }
    }

    /// A commit or a render on this device failed.
    ///
    /// Counted rather than acted on: one failed commit is a frame, and a driver
    /// that refuses a single atomic commit under load is not a driver that has
    /// crashed. The watchdog decides, and it has the count.
    pub fn note_gpu_error(&mut self, index: usize) {
        if let Some(device) = self
            .udev
            .as_mut()
            .and_then(|udev| udev.devices.get_mut(index))
        {
            device.errors = device.errors.saturating_add(1);
        }
        self.watch_gpus();
    }

    /// One turn of the watchdog: look at every GPU, and act on the stuck ones.
    fn gpu_tick(&mut self) {
        self.gpu_watch = false;

        let count = self
            .udev
            .as_ref()
            .map(|udev| udev.devices.len())
            .unwrap_or(0);
        // A device fd is revoked while the VT is switched away, so every flip is
        // outstanding and every commit would fail. That is not a crashed GPU,
        // and recovering it here would fight the session handler for the device.
        let switched_away = self.udev.as_ref().is_some_and(|udev| !udev.active);

        let mut again = false;
        for index in 0..count {
            let Some(udev) = self.udev.as_ref() else {
                return;
            };
            let Some(device) = udev.devices.get(index) else {
                continue;
            };

            if !device.online {
                // Waiting for the card to come back. udev will normally say so
                // and `on_gpu_added` takes it from there; this is the path for
                // when it does not — a driver that rebinds without a fresh add
                // event, or one that arrived before the removal was processed.
                let due = device
                    .offline_since
                    .is_none_or(|at| at.elapsed() >= backoff(device.attempts));
                if due && !switched_away {
                    let card = device.node;
                    self.reopen_device(index, card);
                }
                again = true;
                continue;
            }

            if switched_away {
                again = true;
                continue;
            }

            // A card that has just come back may announce itself before its
            // connectors are readable, and the scan that found nothing then is
            // not repeated by anything: no surface means no flip, no flip means
            // no vblank, and the frame clock does not scan. So the first couple
            // of seconds after a card returns are re-scanned until it has
            // outputs — which is a dark monitor otherwise.
            if device.settle > 0 {
                let empty = device.surfaces.is_empty();
                if let Some(device) = self
                    .udev
                    .as_mut()
                    .and_then(|udev| udev.devices.get_mut(index))
                {
                    device.settle -= 1;
                }
                if empty {
                    self.on_connectors_changed();
                }
                again = true;
                continue;
            }

            let oldest = device
                .surfaces
                .values()
                .filter(|surface| surface.pending)
                .filter_map(|surface| surface.queued_at)
                .map(|at| at.elapsed())
                .max();
            let outstanding = oldest.is_some();
            let errors = device.errors;
            let dwelt = device.stepped_at.map(|at| at.elapsed());

            if stalled(oldest, errors) {
                // Escalate no faster than the hardware can recover. Every tick
                // that finds it still stuck within the dwell keeps the current
                // step in place rather than moving down the ladder.
                let wait = step_wait(device.recovery, device.attempts);
                if may_escalate(dwelt, wait) {
                    self.recover_device(index);
                }
                again = true;
            } else if outstanding {
                // Still within its budget: look again.
                again = true;
            }
        }

        if again {
            self.watch_gpus();
        }
    }

    /// Take the next step on the ladder for this GPU.
    fn recover_device(&mut self, index: usize) {
        let Some(device) = self
            .udev
            .as_mut()
            .and_then(|udev| udev.devices.get_mut(index))
        else {
            return;
        };
        let step = device.recovery.next();
        device.recovery = step;
        device.errors = 0;
        device.stepped_at = Some(std::time::Instant::now());
        let node = device.node;

        // Once per step, not once per tick: a card being reopened every 500ms
        // would otherwise write a line every time.
        tracing::warn!(
            "gpu {index} ({node:?}) stopped responding: {}",
            step.describe()
        );

        match step {
            Step::Healthy => {}
            Step::Resume => self.resume_device(index),
            Step::Reactivate => self.reactivate_device(index),
            Step::Rebuild => self.rebuild_renderer(index),
            Step::Reopen => self.reopen_device(index, node),
        }
    }

    /// A vblank arrived on this device, so whatever was wrong is not wrong now.
    pub fn note_gpu_alive(&mut self, index: usize) {
        if let Some(device) = self
            .udev
            .as_mut()
            .and_then(|udev| udev.devices.get_mut(index))
        {
            if device.recovery != Step::Healthy {
                tracing::info!("gpu {index} ({:?}) is answering again", device.node);
            }
            device.recovery = Step::Healthy;
            device.errors = 0;
            device.attempts = 0;
            device.stepped_at = None;
            device.settle = 0;
        }
    }

    /// Step one: the device is fine, the compositor is waiting for an event
    /// that is not coming.
    fn resume_device(&mut self, index: usize) {
        let ids = self.clear_pending(index);
        for id in ids {
            self.render(id);
        }
    }

    /// Clear every outstanding flip on a device and throw away its damage
    /// history. Returns the outputs to draw again.
    fn clear_pending(&mut self, index: usize) -> Vec<OutputId> {
        let Some(udev) = self.udev.as_mut() else {
            return Vec::new();
        };
        let Some(device) = udev.devices.get_mut(index) else {
            return Vec::new();
        };
        let mut ids = Vec::new();
        for (crtc, surface) in device.surfaces.iter_mut() {
            surface.pending = false;
            surface.queued_at = None;
            // Nothing in the history describes what is on the screen: the frame
            // it was tracking may or may not have been scanned out, and after a
            // reset the answer is not knowable.
            surface.drm_output.reset_buffers();
            ids.push(OutputId {
                device: index,
                crtc: *crtc,
            });
        }
        ids
    }

    /// Step two: take the device back, exactly as a VT switch back does.
    fn reactivate_device(&mut self, index: usize) {
        let Some(udev) = self.udev.as_mut() else {
            return;
        };
        let Some(device) = udev.devices.get_mut(index) else {
            return;
        };
        // `true`: the connectors are disabled first, which forces a full modeset
        // rather than a commit against state the reset invalidated.
        if let Err(e) = device.manager.lock().activate(true) {
            tracing::warn!("gpu {index}: could not take the device back ({e})");
            return;
        }
        let ids = self.clear_pending(index);
        // The reset cleared every gamma ramp, and the client that set one has no
        // way to know.
        self.restore_gamma();
        for id in ids {
            self.render(id);
        }
    }

    /// Step three: the renderer is lost, the card is not.
    ///
    /// A Vulkan device that has taken `VK_ERROR_DEVICE_LOST` fails every
    /// submission from then on. Nothing short of a new `VkDevice` helps, and the
    /// GBM device it was built on is still good — which is why `Device::gbm` is
    /// kept past construction.
    fn rebuild_renderer(&mut self, index: usize) {
        let Some(udev) = self.udev.as_ref() else {
            return;
        };
        let Some(device) = udev.devices.get(index) else {
            return;
        };
        let render_node = device.render_node;

        let rebuilt = {
            let Some(udev) = self.udev.as_mut() else {
                return;
            };
            let device = &mut udev.devices[index];
            crate::udev::build_renderer(&device.gbm, &render_node)
        };
        let mut renderer = match rebuilt {
            Ok(renderer) => renderer,
            Err(e) => {
                tracing::warn!("gpu {index}: no new renderer ({e:#})");
                return;
            }
        };

        // Every surface was built against the old renderer's format list, so
        // they go and are made again by the scan below.
        self.drop_device_outputs(index);
        let Some(udev) = self.udev.as_mut() else {
            return;
        };
        let device = &mut udev.devices[index];
        device.feedback = crate::udev::device_feedback(&render_node, &mut renderer);
        device.renderer = renderer;
        tracing::info!("gpu {index}: a new renderer; bringing its outputs back");
        self.on_connectors_changed();
        // Rebuilt outputs are placed by the scan, same as freshly plugged ones,
        // so the arrangement has to be put back before the file is applied.
        self.restore_output_layout();
        self.apply_output_config();
    }

    /// Step four: close the card and open it again.
    ///
    /// The only thing that helps when the DRM device was unregistered — a bus
    /// reset, or someone unbinding the driver — because every fd on the old one
    /// is answering `ENODEV` and always will.
    ///
    /// `card` is where to look: the node this device was opened on when the
    /// watchdog drives this, or the node udev just announced when a hotplug
    /// does. They differ whenever the card comes back on a new minor.
    fn reopen_device(&mut self, index: usize, card: DrmNode) {
        self.install_device(Some(index), card);
    }

    /// Open a card and put it in the session, either back into the slot it left
    /// or into a new one at the end.
    ///
    /// `slot` is `None` for a GPU this session has not seen. It is appended
    /// rather than fitted into a gap, because `OutputId::device` is an index
    /// into this list and every vblank closure has captured one: the list may
    /// grow, never shift.
    fn install_device(&mut self, slot: Option<usize>, card: DrmNode) -> bool {
        let Some(udev) = self.udev.as_mut() else {
            return false;
        };
        let index = slot.unwrap_or(udev.devices.len());
        let attempts = slot
            .and_then(|slot| udev.devices.get(slot))
            .map(|device| device.attempts)
            .unwrap_or(0);

        // Opened before anything is torn down, and this order is the point.
        //
        // A card that was never unregistered still has an fd open on it here,
        // and a session can only take a device once — logind answers a second
        // `TakeDevice` on the same device with "already taken". So this open can
        // fail for a card that is perfectly fine, and if the outputs had already
        // been dropped and the vblank source already removed to make room for
        // it, that failure would turn a display that was merely stuck into one
        // that is dark with nothing left to drive it. Nothing is given up until
        // there is something to replace it with.
        //
        // The case this rung exists for does not hit that: a card that was
        // unregistered comes back on a fresh minor, which is a different device
        // as far as the seat is concerned, and takes cleanly.
        let render = crate::udev::client_render_node(&card);
        let opened = crate::udev::open_device(&mut udev.session, &card, &render);
        let (manager, mut renderer, gbm, notifier) = match opened {
            Ok(parts) => parts,
            Err(e) => {
                // Not an error: a card that is mid-reset is expected to refuse,
                // and this runs again on a backoff. Said once at warning level
                // when it first happens and at debug after, so a GPU that has
                // truly gone leaves one line rather than a wall of them.
                if attempts < 1 {
                    tracing::warn!("gpu {index}: {card:?} will not open ({e:#}); retrying");
                } else {
                    tracing::debug!("gpu {index}: {card:?} will not open ({e:#})");
                }
                if let Some(device) = slot.and_then(|slot| udev.devices.get_mut(slot)) {
                    device.attempts = device.attempts.saturating_add(1);
                    // Only a slot that was already empty starts the clock. One
                    // that still holds a live card keeps it, along with its
                    // outputs and its vblank source.
                    if !device.online {
                        device.offline_since = Some(std::time::Instant::now());
                    }
                }
                self.watch_gpus();
                return false;
            }
        };

        // Now that there is a replacement: the outputs, then the event source,
        // then the device itself. Each holds a piece of the old fd, and the old
        // fd is what has to go before the minor it occupies can be freed.
        if let Some(index) = slot {
            self.drop_device_outputs(index);
            self.unwatch_device(index);
        }
        let Some(udev) = self.udev.as_mut() else {
            return false;
        };

        let device = Device {
            node: card,
            render_node: render,
            feedback: crate::udev::device_feedback(&render, &mut renderer),
            renderer,
            gbm,
            manager,
            surfaces: std::collections::HashMap::new(),
            online: true,
            bus: bus_id(&card),
            vblank: None,
            recovery: Step::Healthy,
            errors: 0,
            attempts: 0,
            offline_since: None,
            stepped_at: None,
            // Give the connectors a couple of seconds to become readable. See
            // the field.
            settle: 5,
        };
        match slot {
            // Assigning over the slot is what drops the old device — its
            // manager, its renderer and its GBM device all go here, and with
            // them the fd that was pinning a card the kernel had already
            // unregistered.
            Some(slot) => udev.devices[slot] = device,
            None => udev.devices.push(device),
        }

        // Its vblanks carry its own index, for the reason `init` gives: a crtc
        // handle only means something on the device that issued it.
        match self.loop_handle.insert_source(
            notifier,
            move |event, metadata, state: &mut ViewportState| match event {
                smithay::backend::drm::DrmEvent::VBlank(crtc) => state.on_vblank(
                    OutputId {
                        device: index,
                        crtc,
                    },
                    metadata,
                ),
                smithay::backend::drm::DrmEvent::Error(e) => {
                    tracing::error!("drm error on gpu {index}: {e}");
                    state.note_gpu_error(index);
                }
            },
        ) {
            Ok(token) => {
                if let Some(udev) = self.udev.as_mut() {
                    udev.devices[index].vblank = Some(token);
                }
            }
            Err(e) => {
                tracing::warn!("gpu {index}: no vblank source ({e}); its outputs will not draw");
            }
        }

        // DRM master, for the reason `init` claims it: without it the first
        // modeset appears to work and every flip after it fails with EPERM.
        if let Some(udev) = self.udev.as_mut() {
            if let Err(e) = udev.devices[index].manager.lock().activate(false) {
                tracing::error!("gpu {index}: could not claim drm master ({e})");
            }
        }

        tracing::info!("gpu {index}: {card:?} is up; bringing its outputs back");
        // Every device rather than this one: `scan_device` is private to the
        // udev backend, the scan is idempotent for a card that has not changed,
        // and a monitor may well have been moved to another GPU while this one
        // was away.
        self.on_connectors_changed();
        self.restore_output_layout();
        self.apply_output_config();
        self.restore_gamma();
        true
    }

    /// Stop watching a device's vblanks.
    ///
    /// Necessary before the fd goes, and necessary the moment the card is
    /// unregistered whether or not it is going to be reopened: a `Generic`
    /// source on a revoked fd polls ready for ever, which is a compositor
    /// burning a core over a GPU that is not there.
    fn unwatch_device(&mut self, index: usize) {
        let token = self
            .udev
            .as_mut()
            .and_then(|udev| udev.devices.get_mut(index))
            .and_then(|device| device.vblank.take());
        if let Some(token) = token {
            self.loop_handle.remove(token);
        }
    }

    /// Drop every output this GPU was driving.
    ///
    /// The same work `scan_device` does for a monitor that was unplugged, for
    /// every monitor on the card at once: the surface goes, which takes the
    /// `wl_output` global with it, and the output leaves the space so the shell
    /// stops laying windows out on a screen that is not there.
    fn drop_device_outputs(&mut self, index: usize) {
        let gone: Vec<(crtc::Handle, smithay::output::Output)> = self
            .udev
            .as_ref()
            .and_then(|udev| udev.devices.get(index))
            .map(|device| {
                device
                    .surfaces
                    .iter()
                    .map(|(crtc, surface)| (*crtc, surface.output.clone()))
                    .collect()
            })
            .unwrap_or_default();
        if gone.is_empty() {
            return;
        }

        for (crtc, output) in gone {
            if let Some(udev) = self.udev.as_mut() {
                if let Some(device) = udev.devices.get_mut(index) {
                    device.surfaces.remove(&crtc);
                }
            }
            self.space.unmap_output(&output);
            // Whatever named this screen has to stop naming it — see the same
            // handling for an unplugged monitor in `scan_device`.
            if self.active_output.as_deref() == Some(output.name().as_str()) {
                self.active_output = self.space.outputs().next().map(|o| o.name());
            }
            // The dirty set is keyed on an OutputId whose surface has just gone.
            // Left in, the next render pass looks it up, finds nothing and
            // returns — harmless, but it also keeps the frame clock awake.
            self.dirty_outputs.remove(&OutputId {
                device: index,
                crtc,
            });
        }
        self.notify_output_layout();
        self.advertise_outputs();
    }

    /// udev says a DRM device has gone.
    pub fn on_gpu_removed(&mut self, device_id: u64) {
        let found = self.udev.as_ref().and_then(|udev| {
            udev.devices
                .iter()
                .position(|device| device.online && device.node.dev_id() == device_id)
        });
        let Some(index) = found else {
            return;
        };

        tracing::warn!("gpu {index} (dev {device_id}) was removed; its outputs are gone");

        self.drop_device_outputs(index);
        self.unwatch_device(index);
        if let Some(device) = self
            .udev
            .as_mut()
            .and_then(|udev| udev.devices.get_mut(index))
        {
            device.online = false;
            device.recovery = Step::Reopen;
            device.errors = 0;
            device.attempts = 0;
            device.stepped_at = None;
            device.settle = 0;
            device.offline_since = Some(std::time::Instant::now());
        }
        // The card is expected straight back after a bus reset, so the first
        // retry is soon. If it never comes the backoff takes over.
        self.watch_gpus();
    }

    /// udev says a DRM device has appeared.
    ///
    /// Either the one that just went — a card coming back from a bus reset,
    /// which is what an overclocked GPU that could not be reset softly does — or
    /// a GPU that was not there when the session started.
    pub fn on_gpu_added(&mut self, device_id: u64, path: &Path) {
        let Ok(node) = DrmNode::from_dev_id(device_id) else {
            return;
        };
        // Modesetting happens on the primary node. A render node arriving is not
        // a card appearing, and opening one for KMS gets as far as a permission
        // error that looks like a session problem.
        let card = match node.ty() {
            NodeType::Primary => node,
            _ => match node.node_with_type(NodeType::Primary).and_then(|n| n.ok()) {
                Some(card) => card,
                None => return,
            },
        };

        let Some(udev) = self.udev.as_ref() else {
            return;
        };
        // Already driving it.
        if udev
            .devices
            .iter()
            .any(|device| device.online && device.node.dev_id() == card.dev_id())
        {
            return;
        }

        // The slot this card belongs in, by bus address rather than by node
        // path: a card that comes back while an fd still pins its old minor
        // returns on a different one. See `bus_id_of`.
        let bus = bus_id(&card);
        let slot = udev
            .devices
            .iter()
            .position(|device| !device.online && bus.is_some() && device.bus == bus)
            .or_else(|| {
                // No bus address to match on, on either side. One card offline
                // and one card arriving is the same card often enough to be
                // worth taking, and the alternative is a screen that stays dark
                // because sysfs did not answer.
                let offline: Vec<usize> = udev
                    .devices
                    .iter()
                    .enumerate()
                    .filter(|(_, device)| !device.online)
                    .map(|(index, _)| index)
                    .collect();
                (offline.len() == 1).then(|| offline[0])
            });

        if let Some(index) = slot {
            tracing::info!("{card:?} is back at {}; it is gpu {index}", path.display());
            self.reopen_device(index, card);
            return;
        }

        // A GPU this session has not seen.
        tracing::info!(
            "{card:?} appeared at {}; taking it as gpu {}",
            path.display(),
            udev.devices.len()
        );
        self.install_device(None, card);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ladder_escalates_once_per_step() {
        let mut step = Step::Healthy;
        step = step.next();
        assert_eq!(step, Step::Resume);
        step = step.next();
        assert_eq!(step, Step::Reactivate);
        step = step.next();
        assert_eq!(step, Step::Rebuild);
        step = step.next();
        assert_eq!(step, Step::Reopen);
    }

    #[test]
    fn the_bottom_of_the_ladder_keeps_trying() {
        // A card behind a bus reset comes back on its own schedule, and giving
        // up on it would be a monitor that stays dark until the session is
        // restarted.
        assert_eq!(Step::Reopen.next(), Step::Reopen);
    }

    #[test]
    fn a_vblank_within_the_budget_is_not_a_stall() {
        assert!(!stalled(Some(Duration::from_millis(16)), 0));
        assert!(!stalled(None, 0));
    }

    #[test]
    fn a_flip_that_never_came_back_is() {
        assert!(stalled(Some(STALL + Duration::from_millis(1)), 0));
    }

    #[test]
    fn a_failed_commit_is_not_waited_out() {
        // Nothing was queued, so no vblank is coming and no age will ever grow
        // past the threshold. Waiting for one would be waiting forever.
        assert!(stalled(None, 1));
    }

    #[test]
    fn the_first_step_is_taken_at_once() {
        // By the time anything is escalating, the flip has already been
        // outstanding for STALL. Waiting a dwell on top of that would be
        // waiting twice.
        assert!(may_escalate(None, DWELL));
    }

    #[test]
    fn a_step_is_given_time_to_work() {
        // A GPU reset takes seconds. Escalating on every 400ms tick would have
        // the card reopened before the driver had finished resetting it.
        assert!(!may_escalate(Some(Duration::from_millis(400)), DWELL));
        assert!(may_escalate(Some(DWELL), DWELL));
    }

    #[test]
    fn the_repeating_rung_slows_down_and_the_others_do_not() {
        assert_eq!(step_wait(Step::Resume, 9), DWELL);
        assert_eq!(step_wait(Step::Rebuild, 9), DWELL);
        assert!(step_wait(Step::Reopen, 9) > step_wait(Step::Reopen, 0));
    }

    #[test]
    fn the_ladder_outlasts_a_reset() {
        // Four steps, each dwelling, has to cover the slow end of a GPU reset —
        // otherwise the ladder bottoms out on "reopen the card" every time.
        assert!(DWELL * 3 >= Duration::from_secs(4));
    }

    #[test]
    fn the_backoff_starts_quick_and_is_bounded() {
        assert!(backoff(0) < Duration::from_secs(1));
        assert!(backoff(1) > backoff(0));
        assert_eq!(backoff(100), BACKOFF_MAX);
    }

    #[test]
    fn a_card_is_named_by_its_bus_address() {
        assert_eq!(
            bus_id_of(Path::new("../../../0000:03:00.0")).as_deref(),
            Some("0000:03:00.0")
        );
        assert_eq!(
            bus_id_of(Path::new("/sys/devices/platform/soc/fe004000.v3d")).as_deref(),
            Some("fe004000.v3d")
        );
        assert_eq!(bus_id_of(Path::new("")), None);
    }
}
