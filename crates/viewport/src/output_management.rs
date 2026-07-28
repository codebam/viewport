// SPDX-License-Identifier: GPL-3.0-or-later
//
// wlr-output-management-v1: monitor configuration by a client.
//
// This is what kanshi, wlr-randr, wdisplays and nwg-displays speak. Without it
// the only way to move a monitor is the config file and the shell's own IPC,
// which no existing tool knows about — so a laptop docking and undocking has
// nothing watching for it.
//
// Smithay implements none of it, so the dispatch is here; the bindings come
// from wayland-protocols-wlr, which Smithay re-exports.
//
// The protocol is a two-way conversation rather than a set of properties. The
// compositor advertises every head and every mode as its own object, stamped
// with a serial. A client builds a configuration against that serial and asks
// to test or apply it, and a configuration built against a serial that has
// since moved on is cancelled rather than applied — the client was configuring
// a layout that no longer exists, which is exactly what happens when a monitor
// is unplugged while its settings dialogue is open.

use std::sync::{Arc, Mutex};

use smithay::output::{Mode as OutputMode, Output};
use smithay::reexports::wayland_protocols_wlr::output_management::v1::server::{
    zwlr_output_configuration_head_v1::{self, ZwlrOutputConfigurationHeadV1},
    zwlr_output_configuration_v1::{self, ZwlrOutputConfigurationV1},
    zwlr_output_head_v1::{self, AdaptiveSyncState, ZwlrOutputHeadV1},
    zwlr_output_manager_v1::{self, ZwlrOutputManagerV1},
    zwlr_output_mode_v1::{self, ZwlrOutputModeV1},
};
use smithay::reexports::wayland_server::protocol::wl_output;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};
use smithay::utils::{Logical, Point, Transform};

/// The protocol version implemented.
///
/// 4 rather than 3 because that is where `adaptive_sync` arrived, and a client
/// that cannot see variable refresh will happily write a configuration that
/// turns it off without meaning to.
const VERSION: u32 = 4;

/// What an output looks like to a client, gathered before any resource is
/// touched so the advertising code has no reason to reach into the compositor.
#[derive(Debug, Clone)]
pub struct Head {
    pub output: Output,
    pub enabled: bool,
    pub position: Point<i32, Logical>,
    pub adaptive_sync: bool,
}

/// One head's worth of a configuration a client has built.
#[derive(Debug, Clone, Default)]
pub struct HeadChange {
    /// Which output. Named rather than held, because between building the
    /// configuration and applying it the output may be gone.
    pub name: String,
    /// `false` for `disable_head`.
    pub enabled: bool,
    pub mode: Option<OutputMode>,
    /// Whether the mode came from `set_custom_mode` rather than a mode object
    /// the compositor advertised. A custom mode may simply not work, and that
    /// is a different kind of failure from a mode that was offered.
    pub custom_mode: bool,
    pub position: Option<Point<i32, Logical>>,
    pub transform: Option<Transform>,
    pub scale: Option<f64>,
    pub adaptive_sync: Option<bool>,
}

/// What the compositor has to be able to do for a configuration to apply.
pub trait OutputManagementHandler {
    fn output_management_state(&mut self) -> &mut OutputManagementState;

    /// Carry out — or merely check — a configuration.
    ///
    /// Returning `false` fails the configuration, which is what a client shows
    /// as "could not apply". A test must not change anything.
    fn apply_output_configuration(&mut self, changes: &[HeadChange], test_only: bool) -> bool;

    /// The outputs as the protocol needs to see them.
    ///
    /// On the handler because a client that has just bound is told everything
    /// from inside the dispatch, where there is nowhere else to get it.
    fn current_heads(&mut self) -> Vec<Head>;
}

/// Per-manager bookkeeping: the objects handed to one client.
#[derive(Debug, Default)]
struct ManagerData {
    /// Every head and mode object created for this manager, so they can be
    /// finished before a fresh set is sent.
    heads: Vec<ZwlrOutputHeadV1>,
    modes: Vec<ZwlrOutputModeV1>,
    /// Whether the client has said `stop`.
    stopped: bool,
}

/// The global, and everything advertised through it.
#[derive(Debug, Default)]
pub struct OutputManagementState {
    /// Bumped on every `done`. A configuration built against an older one is
    /// cancelled.
    serial: u32,
    managers: Vec<(ZwlrOutputManagerV1, Arc<Mutex<ManagerData>>)>,
}

impl OutputManagementState {
    pub fn new<D>(display: &DisplayHandle) -> Self
    where
        D: GlobalDispatch<ZwlrOutputManagerV1, ()> + 'static,
    {
        display.create_global::<D, ZwlrOutputManagerV1, _>(VERSION, ());
        Self::default()
    }

    /// Tell every client what the outputs are now.
    ///
    /// Everything is torn down and re-sent rather than diffed. A diff is what
    /// the protocol is shaped for, but it is also where this goes wrong
    /// quietly: a head whose mode list changed and whose object was reused
    /// leaves a client holding a mode that no longer exists, and it will
    /// happily configure it. Re-advertising is a handful of messages when a
    /// monitor is plugged in, and nothing at all when nothing changes, because
    /// nothing calls this then.
    pub fn advertise<D>(&mut self, dh: &DisplayHandle, heads: &[Head])
    where
        D: Dispatch<ZwlrOutputHeadV1, HeadData>
            + Dispatch<ZwlrOutputModeV1, ModeData>
            + 'static,
    {
        self.serial = self.serial.wrapping_add(1);
        // Zero is not a serial a client would ever be handed, so it stays free
        // to mean "never advertised" if this ever wraps.
        if self.serial == 0 {
            self.serial = 1;
        }

        // A manager whose client has gone, or which said stop, is not written
        // to again.
        self.managers
            .retain(|(manager, data)| manager.is_alive() && !data.lock().unwrap().stopped);

        for (manager, data) in &self.managers {
            let Some(client) = manager.client() else {
                continue;
            };
            let mut data = data.lock().unwrap();

            // Old objects first. A client is told they are gone before it is
            // shown their replacements, or it cannot tell which set it is
            // looking at.
            for mode in data.modes.drain(..) {
                mode.finished();
            }
            for head in data.heads.drain(..) {
                head.finished();
            }

            for entry in heads {
                let Ok(head) = client.create_resource::<ZwlrOutputHeadV1, HeadData, D>(
                    dh,
                    manager.version(),
                    HeadData {
                        name: entry.output.name(),
                    },
                ) else {
                    continue;
                };
                manager.head(&head);
                let modes = advertise_head::<D>(dh, &client, manager, &head, entry);
                data.modes.extend(modes);
                data.heads.push(head);
            }

            manager.done(self.serial);
        }
    }

    /// Whether a configuration built against `serial` is still describing the
    /// world as it is.
    fn current(&self, serial: u32) -> bool {
        self.serial == serial
    }
}

/// Send one head's state, and the mode objects it needs.
fn advertise_head<D>(
    dh: &DisplayHandle,
    client: &Client,
    manager: &ZwlrOutputManagerV1,
    head: &ZwlrOutputHeadV1,
    entry: &Head,
) -> Vec<ZwlrOutputModeV1>
where
    D: Dispatch<ZwlrOutputModeV1, ModeData> + 'static,
{
    let output = &entry.output;
    let properties = output.physical_properties();

    head.name(output.name());
    head.description(output.description());
    // Millimetres, and zero means unknown — which is what a virtual output
    // has, and what a display that does not say gives us.
    head.physical_size(properties.size.w, properties.size.h);
    if manager.version() >= 2 {
        head.make(properties.make.clone());
        head.model(properties.model.clone());
        // Not carried by Smithay's Output, and an empty string is the
        // protocol's way of saying so. Inventing one would make two identical
        // monitors indistinguishable to a client that keys its profiles on it,
        // which is worse than telling it there is nothing to key on.
        head.serial_number(String::new());
    }

    // The current mode has to be among the advertised ones: `current_mode`
    // names an object, and a mode the compositor is scanning out but never
    // offered leaves a client unable to say "leave it as it is".
    let current = output.current_mode();
    let mut all = output.modes();
    if let Some(current) = current {
        if !all.contains(&current) {
            all.push(current);
        }
    }
    let preferred = output.preferred_mode();

    let mut objects = Vec::new();
    let mut current_object = None;
    for mode in all {
        let Ok(object) = client.create_resource::<ZwlrOutputModeV1, ModeData, D>(
            dh,
            manager.version(),
            ModeData { mode },
        ) else {
            continue;
        };
        head.mode(&object);
        object.size(mode.size.w, mode.size.h);
        // Millihertz, as the protocol and Smithay both count it.
        object.refresh(mode.refresh);
        if preferred == Some(mode) {
            object.preferred();
        }
        if current == Some(mode) {
            current_object = Some(object.clone());
        }
        objects.push(object);
    }

    head.enabled(entry.enabled as i32);
    // Everything below describes a head that is on. The protocol says a
    // disabled head sends none of it, and a client that is told the position
    // of a monitor that is off will draw it in its layout.
    if entry.enabled {
        if let Some(object) = current_object {
            head.current_mode(&object);
        }
        head.position(entry.position.x, entry.position.y);
        head.transform(output.current_transform().into());
        head.scale(output.current_scale().fractional_scale());
        if manager.version() >= 4 {
            head.adaptive_sync(if entry.adaptive_sync {
                AdaptiveSyncState::Enabled
            } else {
                AdaptiveSyncState::Disabled
            });
        }
    }

    objects
}

/// What a head object knows: which output it is.
#[derive(Debug, Clone)]
pub struct HeadData {
    pub name: String,
}

/// What a mode object knows.
#[derive(Debug, Clone)]
pub struct ModeData {
    pub mode: OutputMode,
}

/// A configuration a client is building.
#[derive(Debug)]
pub struct ConfigurationData {
    /// The serial it was built against.
    serial: u32,
    changes: Mutex<Vec<HeadChange>>,
    /// Set by `apply` or `test`, because either may only happen once.
    used: Mutex<bool>,
}

/// One head inside a configuration.
///
/// The changes live in the configuration rather than here, keyed by name, so
/// that applying does not have to walk a set of child objects that may have
/// been destroyed.
#[derive(Debug)]
pub struct ConfigurationHeadData {
    configuration: ZwlrOutputConfigurationV1,
    name: String,
}

impl<D> GlobalDispatch<ZwlrOutputManagerV1, (), D> for OutputManagementState
where
    D: GlobalDispatch<ZwlrOutputManagerV1, ()>
        + Dispatch<ZwlrOutputManagerV1, ()>
        + Dispatch<ZwlrOutputHeadV1, HeadData>
        + Dispatch<ZwlrOutputModeV1, ModeData>
        + Dispatch<ZwlrOutputConfigurationV1, ConfigurationData>
        + Dispatch<ZwlrOutputConfigurationHeadV1, ConfigurationHeadData>
        + OutputManagementHandler
        + 'static,
{
    fn bind(
        state: &mut D,
        dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrOutputManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, D>,
    ) {
        let manager = data_init.init(resource, ());
        let data = Arc::new(Mutex::new(ManagerData::default()));
        state
            .output_management_state()
            .managers
            .push((manager.clone(), data.clone()));

        // A client that has just bound knows nothing, so it is told everything
        // — which is the same message the rest of them get, at the same
        // serial, because a configuration is only valid against the newest.
        let heads = state.current_heads();
        state.output_management_state().advertise::<D>(dh, &heads);
    }
}

impl<D> Dispatch<ZwlrOutputManagerV1, (), D> for OutputManagementState
where
    D: Dispatch<ZwlrOutputManagerV1, ()>
        + Dispatch<ZwlrOutputConfigurationV1, ConfigurationData>
        + Dispatch<ZwlrOutputConfigurationHeadV1, ConfigurationHeadData>
        + OutputManagementHandler
        + 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        manager: &ZwlrOutputManagerV1,
        request: zwlr_output_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            zwlr_output_manager_v1::Request::CreateConfiguration { id, serial } => {
                data_init.init(
                    id,
                    ConfigurationData {
                        serial,
                        changes: Mutex::new(Vec::new()),
                        used: Mutex::new(false),
                    },
                );
            }
            zwlr_output_manager_v1::Request::Stop => {
                manager.finished();
                let managed = state.output_management_state();
                if let Some((_, data)) =
                    managed.managers.iter().find(|(other, _)| other == manager)
                {
                    data.lock().unwrap().stopped = true;
                }
            }
            _ => {}
        }
    }
}

impl<D> Dispatch<ZwlrOutputHeadV1, HeadData, D> for OutputManagementState
where
    D: Dispatch<ZwlrOutputHeadV1, HeadData> + 'static,
{
    fn request(
        _state: &mut D,
        _client: &Client,
        _head: &ZwlrOutputHeadV1,
        _request: zwlr_output_head_v1::Request,
        _data: &HeadData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        // Only `release`, which the destructor handles.
    }
}

impl<D> Dispatch<ZwlrOutputModeV1, ModeData, D> for OutputManagementState
where
    D: Dispatch<ZwlrOutputModeV1, ModeData> + 'static,
{
    fn request(
        _state: &mut D,
        _client: &Client,
        _mode: &ZwlrOutputModeV1,
        _request: zwlr_output_mode_v1::Request,
        _data: &ModeData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
    }
}

impl<D> Dispatch<ZwlrOutputConfigurationV1, ConfigurationData, D> for OutputManagementState
where
    D: Dispatch<ZwlrOutputConfigurationV1, ConfigurationData>
        + Dispatch<ZwlrOutputConfigurationHeadV1, ConfigurationHeadData>
        + OutputManagementHandler
        + 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        configuration: &ZwlrOutputConfigurationV1,
        request: zwlr_output_configuration_v1::Request,
        data: &ConfigurationData,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            zwlr_output_configuration_v1::Request::EnableHead { id, head } => {
                let name = head_name(&head);
                let mut changes = data.changes.lock().unwrap();
                if changes.iter().any(|change| change.name == name) {
                    configuration.post_error(
                        zwlr_output_configuration_v1::Error::AlreadyConfiguredHead,
                        "this head is already in the configuration",
                    );
                    return;
                }
                changes.push(HeadChange {
                    name: name.clone(),
                    enabled: true,
                    ..Default::default()
                });
                data_init.init(
                    id,
                    ConfigurationHeadData {
                        configuration: configuration.clone(),
                        name,
                    },
                );
            }
            zwlr_output_configuration_v1::Request::DisableHead { head } => {
                let name = head_name(&head);
                let mut changes = data.changes.lock().unwrap();
                if changes.iter().any(|change| change.name == name) {
                    configuration.post_error(
                        zwlr_output_configuration_v1::Error::AlreadyConfiguredHead,
                        "this head is already in the configuration",
                    );
                    return;
                }
                changes.push(HeadChange {
                    name,
                    enabled: false,
                    ..Default::default()
                });
            }
            zwlr_output_configuration_v1::Request::Apply
            | zwlr_output_configuration_v1::Request::Test => {
                let test_only =
                    matches!(request, zwlr_output_configuration_v1::Request::Test);
                {
                    let mut used = data.used.lock().unwrap();
                    if *used {
                        configuration.post_error(
                            zwlr_output_configuration_v1::Error::AlreadyUsed,
                            "this configuration has already been applied or tested",
                        );
                        return;
                    }
                    *used = true;
                }

                // Built against a layout that has since changed: the client
                // was configuring monitors that are no longer arranged that
                // way, and applying it would move something it never saw.
                if !state.output_management_state().current(data.serial) {
                    configuration.cancelled();
                    return;
                }

                let changes = data.changes.lock().unwrap().clone();
                if state.apply_output_configuration(&changes, test_only) {
                    configuration.succeeded();
                } else {
                    configuration.failed();
                }
            }
            zwlr_output_configuration_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

impl<D> Dispatch<ZwlrOutputConfigurationHeadV1, ConfigurationHeadData, D>
    for OutputManagementState
where
    D: Dispatch<ZwlrOutputConfigurationHeadV1, ConfigurationHeadData> + 'static,
{
    fn request(
        _state: &mut D,
        _client: &Client,
        head: &ZwlrOutputConfigurationHeadV1,
        request: zwlr_output_configuration_head_v1::Request,
        data: &ConfigurationHeadData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        // The configuration owns the changes; this object only names one of
        // them.
        let Some(configuration) = data.configuration.data::<ConfigurationData>() else {
            return;
        };
        let mut changes = configuration.changes.lock().unwrap();
        let Some(change) = changes.iter_mut().find(|change| change.name == data.name) else {
            return;
        };

        // Each property may be set once. Setting it twice is a protocol error
        // rather than a last-one-wins, because a client that did it does not
        // know what it asked for.
        macro_rules! once {
            ($field:expr, $error:ident) => {
                if $field.is_some() {
                    head.post_error(
                        zwlr_output_configuration_head_v1::Error::$error,
                        "this property is already set on this head",
                    );
                    return;
                }
            };
        }

        match request {
            zwlr_output_configuration_head_v1::Request::SetMode { mode } => {
                once!(change.mode, AlreadySet);
                let Some(data) = mode.data::<ModeData>() else {
                    return;
                };
                change.mode = Some(data.mode);
                change.custom_mode = false;
            }
            zwlr_output_configuration_head_v1::Request::SetCustomMode {
                width,
                height,
                refresh,
            } => {
                once!(change.mode, AlreadySet);
                if width <= 0 || height <= 0 || refresh < 0 {
                    head.post_error(
                        zwlr_output_configuration_head_v1::Error::InvalidCustomMode,
                        "a custom mode must have a positive size",
                    );
                    return;
                }
                change.mode = Some(OutputMode {
                    size: (width, height).into(),
                    refresh,
                });
                change.custom_mode = true;
            }
            zwlr_output_configuration_head_v1::Request::SetPosition { x, y } => {
                once!(change.position, AlreadySet);
                change.position = Some((x, y).into());
            }
            zwlr_output_configuration_head_v1::Request::SetTransform { transform } => {
                once!(change.transform, AlreadySet);
                let Ok(transform) = transform.into_result() else {
                    head.post_error(
                        zwlr_output_configuration_head_v1::Error::InvalidTransform,
                        "not a transform",
                    );
                    return;
                };
                change.transform = Some(transform.into());
            }
            zwlr_output_configuration_head_v1::Request::SetScale { scale } => {
                once!(change.scale, AlreadySet);
                if scale <= 0.0 {
                    head.post_error(
                        zwlr_output_configuration_head_v1::Error::InvalidScale,
                        "a scale must be positive",
                    );
                    return;
                }
                change.scale = Some(scale);
            }
            zwlr_output_configuration_head_v1::Request::SetAdaptiveSync { state } => {
                once!(change.adaptive_sync, AlreadySet);
                let Ok(state) = state.into_result() else {
                    head.post_error(
                        zwlr_output_configuration_head_v1::Error::InvalidAdaptiveSyncState,
                        "not an adaptive sync state",
                    );
                    return;
                };
                change.adaptive_sync = Some(state == AdaptiveSyncState::Enabled);
            }
            _ => {}
        }
    }
}

fn head_name(head: &ZwlrOutputHeadV1) -> String {
    head.data::<HeadData>()
        .map(|data| data.name.clone())
        .unwrap_or_default()
}

/// Wire the dispatch into a compositor state.
#[macro_export]
macro_rules! delegate_output_management {
    ($ty:ty) => {
        smithay::reexports::wayland_server::delegate_global_dispatch!($ty: [
            smithay::reexports::wayland_protocols_wlr::output_management::v1::server::zwlr_output_manager_v1::ZwlrOutputManagerV1: ()
        ] => $crate::output_management::OutputManagementState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            smithay::reexports::wayland_protocols_wlr::output_management::v1::server::zwlr_output_manager_v1::ZwlrOutputManagerV1: ()
        ] => $crate::output_management::OutputManagementState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            smithay::reexports::wayland_protocols_wlr::output_management::v1::server::zwlr_output_head_v1::ZwlrOutputHeadV1: $crate::output_management::HeadData
        ] => $crate::output_management::OutputManagementState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            smithay::reexports::wayland_protocols_wlr::output_management::v1::server::zwlr_output_mode_v1::ZwlrOutputModeV1: $crate::output_management::ModeData
        ] => $crate::output_management::OutputManagementState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            smithay::reexports::wayland_protocols_wlr::output_management::v1::server::zwlr_output_configuration_v1::ZwlrOutputConfigurationV1: $crate::output_management::ConfigurationData
        ] => $crate::output_management::OutputManagementState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            smithay::reexports::wayland_protocols_wlr::output_management::v1::server::zwlr_output_configuration_head_v1::ZwlrOutputConfigurationHeadV1: $crate::output_management::ConfigurationHeadData
        ] => $crate::output_management::OutputManagementState);
    };
}

/// The transform a client sent, as the compositor counts them.
///
/// Separate from the protocol dispatch so it can be tested: the two enums have
/// the same names and different orders is exactly the kind of thing that looks
/// right and rotates the wrong monitor.
pub fn transform_from_wl(transform: wl_output::Transform) -> Transform {
    transform.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_transform_survives_the_round_trip() {
        // A client sends the wl_output number and the compositor stores its
        // own enum. Both have eight variants with the same names, which is why
        // a mismatch here would be invisible until a monitor rotated the wrong
        // way.
        for (wl, ours) in [
            (wl_output::Transform::Normal, Transform::Normal),
            (wl_output::Transform::_90, Transform::_90),
            (wl_output::Transform::_180, Transform::_180),
            (wl_output::Transform::_270, Transform::_270),
            (wl_output::Transform::Flipped, Transform::Flipped),
            (wl_output::Transform::Flipped90, Transform::Flipped90),
            (wl_output::Transform::Flipped180, Transform::Flipped180),
            (wl_output::Transform::Flipped270, Transform::Flipped270),
        ] {
            assert_eq!(transform_from_wl(wl), ours);
            let back: wl_output::Transform = ours.into();
            assert_eq!(back, wl);
        }
    }
}
