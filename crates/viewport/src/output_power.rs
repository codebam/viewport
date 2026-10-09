// SPDX-License-Identifier: GPL-3.0-or-later
//
// wlr-output-power-management-v1: turning a monitor off from a client.
//
// What `wlopm` speaks, and what a settings panel or a lid-close script uses.
// The compositor already turns screens off when the session goes idle; this is
// the same thing on request, which is why it is worth having rather than
// telling people to bind a key (`src/idle.c:199`).
//
// Off here means DPMS off — the planes are cleared and the panel sleeps — and
// not the output being removed from the layout. A monitor that is asleep is
// still where it was, and windows do not move off it, which is the difference
// between this and disabling an output through wlr-output-management.
//
// Smithay implements none of it, so the dispatch is here.

use std::collections::HashMap;

use smithay::output::Output;
use smithay::reexports::wayland_protocols_wlr::output_power_management::v1::server::{
    zwlr_output_power_manager_v1::{self, ZwlrOutputPowerManagerV1},
    zwlr_output_power_v1::{self, Mode, ZwlrOutputPowerV1},
};
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New,
};

use crate::state::trusted_native;

/// The state a watcher should be told, given an output's own switch and the
/// session-wide blank that overrides it.
///
/// The DRM backend's last gate before a frame is queued is
/// `blanked || !powered`, so the answer given to a client has to combine the
/// two the same way. They used to be independent: `changed` ran only for a
/// client-requested mode, so an idle blank left a watcher reading On while
/// every panel was off.
pub fn effective_output_power(powered: bool, blanked: bool) -> bool {
    powered && !blanked
}

/// What the compositor has to be able to do for the request to mean anything.
pub trait OutputPowerHandler {
    fn output_power_state(&mut self) -> &mut OutputPowerState;

    /// Turn one monitor's backlight off, or on again.
    fn set_output_power(&mut self, output: &Output, on: bool);

    /// Whether it is on now, so a client is told the truth when it binds.
    fn output_power(&mut self, output: &Output) -> bool;
}

/// The global, and who holds each output.
#[derive(Debug, Default)]
pub struct OutputPowerState {
    /// One client at a time per output, as the protocol requires: two clients
    /// disagreeing about whether a monitor is on has no answer.
    ///
    /// Keyed by [`Output`] identity rather than by name. A name outlives the
    /// monitor that wore it — unplug a DP-1, plug in another, and the new head
    /// is DP-1 again — and a name-keyed entry left behind by the old one kept
    /// the name: every new client for the new monitor was refused for ever,
    /// and `changed` matched by name so the stale control received mode events
    /// about a screen it did not own. Identity keys make a replugged head a
    /// new output, and [`Self::output_gone`] reaps the entry of the one that
    /// went.
    // Hashing `Output` hashes the `Arc` behind it, which never moves: the
    // interior mutability clippy sees in there cannot change a key.
    #[allow(clippy::mutable_key_type)]
    controls: HashMap<Output, ZwlrOutputPowerV1>,
}

impl OutputPowerState {
    pub fn new<D>(display: &DisplayHandle) -> Self
    where
        D: GlobalDispatch<ZwlrOutputPowerManagerV1, ()> + 'static,
    {
        display.create_global::<D, ZwlrOutputPowerManagerV1, _>(1, ());
        Self::default()
    }

    /// Tell whoever is watching that a monitor changed.
    ///
    /// The idle timer turns screens off on its own, and a client holding a
    /// control is entitled to know without asking.
    pub fn changed(&self, output: &Output, on: bool) {
        let Some(control) = self.controls.get(output) else {
            return;
        };
        control.mode(if on { Mode::On } else { Mode::Off });
    }

    /// A head went away: drop the control still held for its name and tell
    /// whoever held it.
    ///
    /// Called from `output_removed`. Entries used to leave this map only when
    /// the client destroyed its control object, so a long-lived client — a
    /// settings panel keeps its control for the life of the session — held a
    /// name across unplug and replug. `failed` is what the protocol has for
    /// "this control is finished", and dropping the entry means its `set_mode`
    /// requests stop answering for hardware that is no longer there.
    ///
    /// By name, because `output_removed` is handed the name. That is safe
    /// here: it runs for the head that is going, names are unique among live
    /// heads, and the pass that creates a replacement names it after this one
    /// has been reaped — so no live control can be caught by it.
    pub fn output_gone(&mut self, name: &str) {
        self.forget(name, None);
    }

    /// Fail and drop every control of `name`'s except one held against `keep`.
    ///
    /// `keep` is the head taking the name over when a genuinely new `Output`
    /// reuses one a dead head left behind: the stale control is told and
    /// dropped rather than left failing every future client, which is the
    /// replace-and-fail-the-old answer `gamma` has always had to a second
    /// claimant. The same-Output case is not this — it is refused in the
    /// request handler, as the protocol says.
    fn forget(&mut self, name: &str, keep: Option<&Output>) {
        let stale: Vec<Output> = self
            .controls
            .keys()
            .filter(|held| Some(*held) != keep && held.name() == name)
            .cloned()
            .collect();
        for held in stale {
            if let Some(control) = self.controls.remove(&held) {
                control.failed();
            }
        }
    }
}

/// An `Output` with no screen behind it, for a control object that exists only
/// to be failed.
///
/// A `ZwlrOutputPowerV1` that is created has to be initialised before its
/// request handler returns — wayland-backend treats an unbound `New` id as a
/// fatal bug — and the protocol's `failed` event carries no payload, so the
/// name here is never seen by anyone.
fn unbacked_output() -> Output {
    Output::new(
        "gone".to_owned(),
        smithay::output::PhysicalProperties {
            size: (0, 0).into(),
            subpixel: smithay::output::Subpixel::Unknown,
            make: String::new(),
            model: String::new(),
            serial_number: String::new(),
        },
    )
}

/// What a control object knows.
#[derive(Debug)]
pub struct ControlData {
    pub output: Output,
}

impl<D> GlobalDispatch<ZwlrOutputPowerManagerV1, (), D> for OutputPowerState
where
    D: GlobalDispatch<ZwlrOutputPowerManagerV1, ()>
        + Dispatch<ZwlrOutputPowerManagerV1, ()>
        + Dispatch<ZwlrOutputPowerV1, ControlData>
        + OutputPowerHandler
        + 'static,
{
    /// Turning a monitor off is a whole-session action, not an application
    /// one: a sandboxed client is not told the global exists.
    fn can_view(client: Client, _global_data: &()) -> bool {
        trusted_native(&client)
    }

    fn bind(
        _state: &mut D,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrOutputPowerManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, D>,
    ) {
        data_init.init(resource, ());
    }
}

impl<D> Dispatch<ZwlrOutputPowerManagerV1, (), D> for OutputPowerState
where
    D: Dispatch<ZwlrOutputPowerManagerV1, ()>
        + Dispatch<ZwlrOutputPowerV1, ControlData>
        + OutputPowerHandler
        + 'static,
{
    fn request(
        state: &mut D,
        client: &Client,
        _manager: &ZwlrOutputPowerManagerV1,
        request: zwlr_output_power_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        let zwlr_output_power_manager_v1::Request::GetOutputPower { id, output } = request else {
            return;
        };

        // Every `New` id has to be initialised before this returns, refused
        // paths included: wayland-backend treats an id the handler left
        // unbound as a fatal bug. `can_view` keeps a sandboxed client from
        // binding the manager in the first place, but it does not run per
        // request, so the trust decision is repeated here — and its refusal
        // binds the object and fails it instead of returning early. No control
        // is registered either way.
        if !trusted_native(client) {
            tracing::debug!("output-power: ignoring a request from a sandboxed client");
            let control = data_init.init(
                id,
                ControlData {
                    output: unbacked_output(),
                },
            );
            control.failed();
            return;
        }

        let Some(output) = Output::from_resource(&output) else {
            // The monitor went between the client looking it up and asking.
            // There is no error on the manager for it, so the control is
            // created and immediately failed.
            let control = data_init.init(
                id,
                ControlData {
                    output: unbacked_output(),
                },
            );
            control.failed();
            return;
        };

        let name = output.name();
        let on = state.output_power(&output);
        let control = data_init.init(
            id,
            ControlData {
                output: output.clone(),
            },
        );

        // A head that took this name over from one that went without being
        // reaped: the stale control is failed and dropped — gamma's
        // replace-and-fail-the-old — so its holder knows to ask again and the
        // name is free for the monitor that now wears it.
        state.output_power_state().forget(&name, Some(&output));

        // Someone already has this monitor. The newcomer is told rather than
        // left waiting for a mode event that will go to the other client. The
        // test is Output identity, so a control left over from a dead head of
        // the same name cannot reach across and refuse this one.
        if state.output_power_state().controls.contains_key(&output) {
            control.failed();
            return;
        }

        // The state it is in, before anything is asked of it.
        control.mode(if on { Mode::On } else { Mode::Off });
        state
            .output_power_state()
            .controls
            .insert(output, control.clone());
    }
}

impl<D> Dispatch<ZwlrOutputPowerV1, ControlData, D> for OutputPowerState
where
    D: Dispatch<ZwlrOutputPowerV1, ControlData> + OutputPowerHandler + 'static,
{
    fn request(
        state: &mut D,
        client: &Client,
        control: &ZwlrOutputPowerV1,
        request: zwlr_output_power_v1::Request,
        data: &ControlData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        if !trusted_native(client) {
            tracing::debug!("output-power: ignoring a mode request from a sandboxed client");
            return;
        }
        let zwlr_output_power_v1::Request::SetMode { mode } = request else {
            return;
        };
        let Ok(mode) = mode.into_result() else {
            return;
        };

        // Not from a client that lost the output to someone else.
        if state.output_power_state().controls.get(&data.output) != Some(control) {
            return;
        }

        let on = mode == Mode::On;
        state.set_output_power(&data.output, on);
        // Confirmed rather than assumed: the protocol says the compositor
        // reports the mode it ended up in, and a display that refused is a
        // display the client should not be told is off.
        let now = state.output_power(&data.output);
        control.mode(if now { Mode::On } else { Mode::Off });
    }

    fn destroyed(
        state: &mut D,
        _client: smithay::reexports::wayland_server::backend::ClientId,
        control: &ZwlrOutputPowerV1,
        data: &ControlData,
    ) {
        let managed = state.output_power_state();
        if managed.controls.get(&data.output) == Some(control) {
            managed.controls.remove(&data.output);
        }
        // The monitor is left as it is. A client that turned a screen off and
        // exited meant it — this is not a lease, and turning it back on would
        // undo the one thing the client was asked to do.
    }
}

/// Wire the dispatch into a compositor state.
#[macro_export]
macro_rules! delegate_output_power {
    ($ty:ty) => {
        smithay::reexports::wayland_server::delegate_global_dispatch!($ty: [
            smithay::reexports::wayland_protocols_wlr::output_power_management::v1::server::zwlr_output_power_manager_v1::ZwlrOutputPowerManagerV1: ()
        ] => $crate::output_power::OutputPowerState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            smithay::reexports::wayland_protocols_wlr::output_power_management::v1::server::zwlr_output_power_manager_v1::ZwlrOutputPowerManagerV1: ()
        ] => $crate::output_power::OutputPowerState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            smithay::reexports::wayland_protocols_wlr::output_power_management::v1::server::zwlr_output_power_v1::ZwlrOutputPowerV1: $crate::output_power::ControlData
        ] => $crate::output_power::OutputPowerState);
    };
}

#[cfg(test)]
mod tests {
    use super::effective_output_power;

    /// The four combinations the backend gate can see: a session blank has to
    /// win over a client's On, and a client's Off has to stay Off when the
    /// blank lifts.
    #[test]
    fn a_session_blank_overrides_a_client_requested_on() {
        assert!(effective_output_power(true, false));
        assert!(!effective_output_power(true, true));
        assert!(!effective_output_power(false, false));
        assert!(!effective_output_power(false, true));
    }

    /// The bug this guards is not reachable without two plugs of the same
    /// connector: controls keyed by name and reaped only when the client
    /// destroyed its object. A settings panel holds its control for the
    /// session, so after unplug plus replug the stale entry refused every new
    /// client for the new monitor and `changed` matched it by name, sending it
    /// events about a screen it did not own. The wayland objects cannot be
    /// built in a unit test, so — like the blank-path guard in
    /// `output_control_tests` — the wiring is checked against this source.
    #[test]
    fn a_replugged_head_is_not_held_again_by_the_old_control() {
        let source = include_str!("output_power.rs");
        let source = source.split("#[cfg(test)]").next().unwrap_or(source);
        assert!(
            source.contains("controls: HashMap<Output, ZwlrOutputPowerV1>"),
            "controls must be keyed by Output identity, not by name"
        );
        assert!(
            source.contains("pub fn output_gone(&mut self, name: &str)"),
            "head removal has to drop the control left behind for the name"
        );
        assert!(
            source.contains("state.output_power_state().forget(&name, Some(&output))"),
            "a new Output reusing a name must fail the stale control, gamma-style"
        );
        assert!(
            source.contains("controls.get(&data.output) != Some(control)"),
            "ownership has to be tested by Output identity, not by name"
        );
    }
}
