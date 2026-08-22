// SPDX-License-Identifier: GPL-3.0-or-later
//
// wlr-foreign-toplevel-management-v1: the window list, and acting on it.
//
// The compositor already publishes ext-foreign-toplevel-list, which is the
// newer, read-only protocol — a screen-share picker can see the windows. What
// that one cannot do is ask for anything, and asking is the point: `rofi -show
// window`, wofi, wlrctl and every taskbar with a click-to-focus list need to
// say "focus that one" or "close that one". An alt-tab replacement written as
// an ordinary client cannot work without it.
//
// Smithay implements the read-only half and not this one, so the dispatch is
// here. Both describe the same windows, because a client that binds only one
// of them must not see a different desktop from a client that binds the other.
//
// What is deliberately not implemented: maximize, minimize and set_rectangle.
// This compositor has no notion of either state — the shell owns the layout,
// and a window is where the shell put it — so accepting the request and doing
// nothing is the honest answer, which is also what the C build does. Fullscreen
// is forwarded to the shell rather than applied here for the same reason
// (`src/foreign.c:65`).

use std::collections::HashMap;

use smithay::output::Output;
use smithay::reexports::wayland_protocols_wlr::foreign_toplevel::v1::server::{
    zwlr_foreign_toplevel_handle_v1::{self, State as HandleState, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
};
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};

/// Version 3: `parent` arrived there, and a taskbar that cannot see which
/// window a dialogue belongs to lists it as though it were its own program.
const VERSION: u32 = 3;

/// One window, as the outside sees it.
#[derive(Debug, Clone, Default)]
pub struct Toplevel {
    pub title: String,
    pub app_id: String,
    pub activated: bool,
    pub fullscreen: bool,
    /// The outputs it is on, so a taskbar per monitor can show its own
    /// windows.
    pub outputs: Vec<Output>,
}

/// What the compositor has to be able to do for a request to mean anything.
pub trait ForeignToplevelHandler {
    fn foreign_toplevel_state(&mut self) -> &mut ForeignToplevelState;

    /// Focus a window. The request names a seat, but there is only one here,
    /// so any request is for ours (`src/foreign.c:50`).
    fn activate_toplevel(&mut self, id: u32);

    /// Ask it to close. Asking is all a compositor may do: a client is
    /// entitled to put up "save your work?" instead.
    fn close_toplevel(&mut self, id: u32);

    /// Fullscreen is the shell's decision — it owns the layout and the bar —
    /// so this forwards rather than applies, and the state comes back the
    /// ordinary way.
    fn fullscreen_toplevel(&mut self, id: u32, fullscreen: bool);
}

/// The global, the windows, and the handles each client holds for them.
#[derive(Debug, Default)]
pub struct ForeignToplevelState {
    managers: Vec<ZwlrForeignToplevelManagerV1>,
    /// Keyed by view id, which is what every request comes back as.
    toplevels: HashMap<u32, Toplevel>,
    /// One handle per manager per window.
    handles: HashMap<u32, Vec<ZwlrForeignToplevelHandleV1>>,
}

impl ForeignToplevelState {
    pub fn new<D>(display: &DisplayHandle) -> Self
    where
        D: GlobalDispatch<ZwlrForeignToplevelManagerV1, ()> + 'static,
    {
        display.create_global::<D, ZwlrForeignToplevelManagerV1, _>(VERSION, ());
        Self::default()
    }

    /// A window has appeared.
    ///
    /// `outputs` is what it is on at the moment of announcing — usually
    /// nothing, because a window is announced when its client first commits,
    /// and it is placed only once the shell has answered — and
    /// [`ForeignToplevelState::set_outputs`] keeps it true afterwards, as the
    /// shell moves it between screens. Without both halves a taskbar drawn
    /// per monitor cannot tell which of its lists a window belongs in.
    pub fn add<D>(
        &mut self,
        dh: &DisplayHandle,
        id: u32,
        title: &str,
        app_id: &str,
        outputs: Vec<Output>,
    ) where
        D: Dispatch<ZwlrForeignToplevelHandleV1, HandleData> + 'static,
    {
        let toplevel = Toplevel {
            title: title.to_owned(),
            app_id: app_id.to_owned(),
            outputs,
            ..Default::default()
        };
        self.toplevels.insert(id, toplevel.clone());
        self.handles.entry(id).or_default();

        self.managers.retain(|manager| manager.is_alive());
        let managers = self.managers.clone();
        for manager in managers {
            self.publish::<D>(dh, &manager, id, &toplevel);
        }
    }

    /// Its title or app id changed.
    pub fn update(&mut self, id: u32, title: &str, app_id: &str) {
        let Some(toplevel) = self.toplevels.get_mut(&id) else {
            return;
        };
        if toplevel.title == title && toplevel.app_id == app_id {
            return;
        }
        toplevel.title = title.to_owned();
        toplevel.app_id = app_id.to_owned();

        for handle in self.handles.get(&id).into_iter().flatten() {
            handle.title(title.to_owned());
            handle.app_id(app_id.to_owned());
            handle.done();
        }
    }

    /// It was focused, or fullscreened.
    pub fn set_state(&mut self, id: u32, activated: bool, fullscreen: bool) {
        let Some(toplevel) = self.toplevels.get_mut(&id) else {
            return;
        };
        if toplevel.activated == activated && toplevel.fullscreen == fullscreen {
            return;
        }
        toplevel.activated = activated;
        toplevel.fullscreen = fullscreen;
        let states = state_bytes(activated, fullscreen);

        for handle in self.handles.get(&id).into_iter().flatten() {
            handle.state(states.clone());
            handle.done();
        }
    }

    /// It is gone.
    ///
    /// `closed` rather than a silent destroy: a client that is not told keeps
    /// the window in its list for ever, and clicking it does nothing.
    pub fn remove(&mut self, id: u32) {
        self.toplevels.remove(&id);
        for handle in self.handles.remove(&id).into_iter().flatten() {
            handle.closed();
        }
    }

    /// Which outputs the window is on now.
    ///
    /// The difference goes out per handle — `output_leave` for the screens it
    /// has left and `output_enter` for the ones it has arrived on, under one
    /// `done` so no client ever sees half a move. This is the update half of
    /// [`ForeignToplevelState::add`]: announce says where a window starts,
    /// this keeps that true as the shell re-layouts.
    pub fn set_outputs(&mut self, id: u32, outputs: Vec<Output>) {
        let Some(toplevel) = self.toplevels.get_mut(&id) else {
            return;
        };
        let previous = std::mem::take(&mut toplevel.outputs);
        if previous == outputs {
            // Put it back rather than storing the caller's fresh list: equal
            // or not is all this needs to know about it.
            toplevel.outputs = previous;
            return;
        }
        let entered: Vec<Output> = outputs
            .iter()
            .filter(|o| !previous.contains(o))
            .cloned()
            .collect();
        let left: Vec<Output> = previous
            .iter()
            .filter(|o| !outputs.contains(o))
            .cloned()
            .collect();
        toplevel.outputs = outputs;

        for handle in self.handles.get(&id).into_iter().flatten() {
            let Some(client) = handle.client() else {
                continue;
            };
            for output in &left {
                for resource in output.client_outputs(&client) {
                    handle.output_leave(&resource);
                }
            }
            for output in &entered {
                for resource in output.client_outputs(&client) {
                    handle.output_enter(&resource);
                }
            }
            handle.done();
        }
    }

    fn publish<D>(
        &mut self,
        dh: &DisplayHandle,
        manager: &ZwlrForeignToplevelManagerV1,
        id: u32,
        toplevel: &Toplevel,
    ) where
        D: Dispatch<ZwlrForeignToplevelHandleV1, HandleData> + 'static,
    {
        let Some(client) = manager.client() else {
            return;
        };
        let Ok(handle) = client.create_resource::<ZwlrForeignToplevelHandleV1, HandleData, D>(
            dh,
            manager.version(),
            HandleData { id },
        ) else {
            return;
        };
        manager.toplevel(&handle);
        handle.title(toplevel.title.clone());
        handle.app_id(toplevel.app_id.clone());
        for output in &toplevel.outputs {
            for resource in output.client_outputs(&client) {
                handle.output_enter(&resource);
            }
        }
        handle.state(state_bytes(toplevel.activated, toplevel.fullscreen));
        // Nothing a client has been told is real until done: the protocol
        // batches, and a taskbar that acted on a half-described window would
        // show one with no title.
        handle.done();
        self.handles.entry(id).or_default().push(handle);
    }
}

/// The state array, as the protocol carries it: native-endian u32s.
fn state_bytes(activated: bool, fullscreen: bool) -> Vec<u8> {
    let mut states = Vec::new();
    if activated {
        states.extend((HandleState::Activated as u32).to_ne_bytes());
    }
    if fullscreen {
        states.extend((HandleState::Fullscreen as u32).to_ne_bytes());
    }
    states
}

/// What a handle knows: which window it is.
#[derive(Debug, Clone)]
pub struct HandleData {
    pub id: u32,
}

impl<D> GlobalDispatch<ZwlrForeignToplevelManagerV1, (), D> for ForeignToplevelState
where
    D: GlobalDispatch<ZwlrForeignToplevelManagerV1, ()>
        + Dispatch<ZwlrForeignToplevelManagerV1, ()>
        + Dispatch<ZwlrForeignToplevelHandleV1, HandleData>
        + ForeignToplevelHandler
        + 'static,
{
    fn bind(
        state: &mut D,
        dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrForeignToplevelManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, D>,
    ) {
        let manager = data_init.init(resource, ());
        let managed = state.foreign_toplevel_state();
        managed.managers.push(manager.clone());

        // Every window that already exists, because a taskbar started after
        // the session shows an empty list otherwise.
        let existing: Vec<(u32, Toplevel)> = managed
            .toplevels
            .iter()
            .map(|(id, toplevel)| (*id, toplevel.clone()))
            .collect();
        for (id, toplevel) in existing {
            state
                .foreign_toplevel_state()
                .publish::<D>(dh, &manager, id, &toplevel);
        }
    }
}

impl<D> Dispatch<ZwlrForeignToplevelManagerV1, (), D> for ForeignToplevelState
where
    D: Dispatch<ZwlrForeignToplevelManagerV1, ()> + ForeignToplevelHandler + 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        manager: &ZwlrForeignToplevelManagerV1,
        request: zwlr_foreign_toplevel_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        if !matches!(request, zwlr_foreign_toplevel_manager_v1::Request::Stop) {
            return;
        }
        manager.finished();
        state
            .foreign_toplevel_state()
            .managers
            .retain(|other| other != manager);
    }
}

impl<D> Dispatch<ZwlrForeignToplevelHandleV1, HandleData, D> for ForeignToplevelState
where
    D: Dispatch<ZwlrForeignToplevelHandleV1, HandleData> + ForeignToplevelHandler + 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        _handle: &ZwlrForeignToplevelHandleV1,
        request: zwlr_foreign_toplevel_handle_v1::Request,
        data: &HandleData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            zwlr_foreign_toplevel_handle_v1::Request::Activate { .. } => {
                state.activate_toplevel(data.id)
            }
            zwlr_foreign_toplevel_handle_v1::Request::Close => {
                state.close_toplevel(data.id)
            }
            zwlr_foreign_toplevel_handle_v1::Request::SetFullscreen { .. } => {
                state.fullscreen_toplevel(data.id, true)
            }
            zwlr_foreign_toplevel_handle_v1::Request::UnsetFullscreen => {
                state.fullscreen_toplevel(data.id, false)
            }
            // Accepted and not acted on. This compositor has no notion of
            // either state — the shell owns the layout — and there is nothing
            // to report back, so the client's own list stays as it was.
            zwlr_foreign_toplevel_handle_v1::Request::SetMaximized
            | zwlr_foreign_toplevel_handle_v1::Request::UnsetMaximized
            | zwlr_foreign_toplevel_handle_v1::Request::SetMinimized
            | zwlr_foreign_toplevel_handle_v1::Request::UnsetMinimized
            // Where a taskbar drew the window's entry, for a minimise
            // animation there is nothing to animate.
            | zwlr_foreign_toplevel_handle_v1::Request::SetRectangle { .. } => {}
            zwlr_foreign_toplevel_handle_v1::Request::Destroy => {}
            _ => {}
        }
    }

    fn destroyed(
        state: &mut D,
        _client: smithay::reexports::wayland_server::backend::ClientId,
        handle: &ZwlrForeignToplevelHandleV1,
        data: &HandleData,
    ) {
        if let Some(handles) = state.foreign_toplevel_state().handles.get_mut(&data.id) {
            handles.retain(|other| other != handle);
        }
    }
}

/// Wire the dispatch into a compositor state.
#[macro_export]
macro_rules! delegate_foreign_toplevel {
    ($ty:ty) => {
        smithay::reexports::wayland_server::delegate_global_dispatch!($ty: [
            smithay::reexports::wayland_protocols_wlr::foreign_toplevel::v1::server::zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1: ()
        ] => $crate::foreign_toplevel::ForeignToplevelState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            smithay::reexports::wayland_protocols_wlr::foreign_toplevel::v1::server::zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1: ()
        ] => $crate::foreign_toplevel::ForeignToplevelState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            smithay::reexports::wayland_protocols_wlr::foreign_toplevel::v1::server::zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1: $crate::foreign_toplevel::HandleData
        ] => $crate::foreign_toplevel::ForeignToplevelState);
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_state_array_is_a_list_of_u32s() {
        // The protocol says an array of the enum's values, native-endian. A
        // client reads it four bytes at a time, so a byte-per-state array
        // would be read as one enormous state number.
        assert!(state_bytes(false, false).is_empty());
        assert_eq!(state_bytes(true, false).len(), 4);
        assert_eq!(state_bytes(true, true).len(), 8);
        assert_eq!(
            state_bytes(true, false),
            (HandleState::Activated as u32).to_ne_bytes().to_vec()
        );
    }

    #[test]
    fn fullscreen_comes_after_activated() {
        // Order is not significant to the protocol, but a client reading the
        // first entry only — and they exist — should see the state that
        // decides how it is drawn in a list.
        let bytes = state_bytes(true, true);
        assert_eq!(&bytes[..4], (HandleState::Activated as u32).to_ne_bytes());
        assert_eq!(&bytes[4..], (HandleState::Fullscreen as u32).to_ne_bytes());
    }
}
