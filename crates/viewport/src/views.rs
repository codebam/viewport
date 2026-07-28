// SPDX-License-Identifier: GPL-3.0-or-later
//
// The window registry. Ports the toplevel bookkeeping in src/xdg_shell.c and
// src/view.c.
//
// This is the source of truth for what windows exist, deliberately separate
// from Smithay's `Space`. A window lives here from the moment the client
// creates it, but it is only mapped into the Space once the shell has sent a
// `view.layout` for it — because in Viewport the compositor has no layout
// policy at all, and a window with no shell-assigned rectangle has nowhere it
// could legitimately be drawn.

use smithay::desktop::Window;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::xdg::{SurfaceCachedState, XdgToplevelSurfaceData};

use viewport_ipc::event::ViewAdded;
use viewport_ipc::Box;

/// The id of no window. `view.focused` carries this when focus leaves every
/// client (`src/input.c:163`), so real ids start at 1.
pub const NO_VIEW: u32 = 0;

pub struct View {
    pub id: u32,
    pub window: Window,

    /// The client has mapped it: it committed a buffer and is ready to be
    /// shown. Until then the shell is not told about it at all.
    pub mapped: bool,

    /// The shell has given it a rectangle, so it is in the `Space`. A window
    /// can be mapped without being placed — that is the whole window between
    /// `view.added` going out and `view.layout` coming back.
    pub placed: bool,

    /// The last geometry the shell asked for. Absent fields in the next
    /// `view.layout` resolve against this (`src/ipc.c:833`).
    pub box_: Box,

    pub visible: bool,
    pub scale: f64,
    pub clip: Option<Box>,

    /// Driven a frame at a time by a tween in the shell, which cannot fade a
    /// window with CSS: the frame is DOM, the contents are a surface the
    /// compositor draws.
    pub opacity: f32,

    /// The size the client was last configured with, clamped.
    ///
    /// Kept so a move does not cost a resize. Every configure is a round trip,
    /// and the shell resends the whole rectangle on every frame of an
    /// animation — a window sliding across the screen changes position sixty
    /// times a second and its size not at all (`src/xdg_shell.c:877`).
    pub configured: Option<(i32, i32)>,
}

impl View {
    /// The `view.added` payload for this window.
    ///
    /// `replay` distinguishes a window that just appeared from one being
    /// re-announced in answer to `view.query`, which is how a reloading shell
    /// rebuilds its tree without the windows looking new.
    pub fn added(&self, output: String, replay: bool) -> ViewAdded {
        let (min_width, min_height) = self.min_size();
        let (width, height) = self.natural_size();
        ViewAdded {
            id: self.id,
            title: self.title(),
            app_id: self.app_id(),
            output,
            min_width,
            min_height,
            replay,
            floating: self.wants_floating(),
            width,
            height,
        }
    }

    pub fn surface(&self) -> Option<WlSurface> {
        self.window.toplevel().map(|t| t.wl_surface().clone())
    }

    pub fn title(&self) -> String {
        // Two layers of Option: the surface may have no role attributes, and
        // the client may not have set a title.
        self.role_attribute(|attrs| attrs.title.clone())
            .flatten()
            .unwrap_or_default()
    }

    pub fn app_id(&self) -> String {
        self.role_attribute(|attrs| attrs.app_id.clone())
            .flatten()
            .unwrap_or_default()
    }

    /// The client's minimum size, so the shell can refuse to shrink a window
    /// past what it accepts. Zero on an axis means unconstrained.
    pub fn min_size(&self) -> (i32, i32) {
        let Some(surface) = self.surface() else {
            return (0, 0);
        };
        with_states(&surface, |states| {
            let mut guard = states.cached_state.get::<SurfaceCachedState>();
            let min = guard.current().min_size;
            (min.w, min.h)
        })
    }

    /// What a floating window should open at.
    pub fn natural_size(&self) -> (i32, i32) {
        let size = self.window.geometry().size;
        (size.w, size.h)
    }

    /// Whether this window would rather float than be tiled.
    ///
    /// The compositor can see the signals for this and the shell cannot: a
    /// parent toplevel means a dialog.
    ///
    /// The C build also consults the X11 window type, and xdg-dialog-v1 would
    /// be a third signal — but Smithay keeps `ToplevelDialogHint` private, so
    /// reading it needs an upstream change. Parent alone covers the ordinary
    /// dialog case.
    pub fn wants_floating(&self) -> bool {
        self.window
            .toplevel()
            .is_some_and(|toplevel| toplevel.parent().is_some())
    }

    fn role_attribute<F, T>(&self, f: F) -> Option<T>
    where
        F: FnOnce(&smithay::wayland::shell::xdg::XdgToplevelSurfaceRoleAttributes) -> T,
    {
        let surface = self.surface()?;
        with_states(&surface, |states| {
            let data = states.data_map.get::<XdgToplevelSurfaceData>()?;
            let guard = data.lock().ok()?;
            Some(f(&guard))
        })
    }
}

#[derive(Default)]
pub struct Views {
    next_id: u32,
    views: Vec<View>,
}

impl Views {
    pub fn new() -> Self {
        // src/server.c:114. Starting at 1 keeps 0 free as the "nothing focused"
        // sentinel.
        Self {
            next_id: 1,
            views: Vec::new(),
        }
    }

    pub fn insert(&mut self, window: Window) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        self.views.push(View {
            id,
            window,
            mapped: false,
            placed: false,
            box_: Box::new(0, 0, 0, 0),
            visible: true,
            scale: 1.0,
            clip: None,
            opacity: 1.0,
            configured: None,
        });
        id
    }

    pub fn remove(&mut self, id: u32) -> Option<View> {
        let index = self.views.iter().position(|v| v.id == id)?;
        Some(self.views.remove(index))
    }

    pub fn get(&self, id: u32) -> Option<&View> {
        self.views.iter().find(|v| v.id == id)
    }

    pub fn get_mut(&mut self, id: u32) -> Option<&mut View> {
        self.views.iter_mut().find(|v| v.id == id)
    }

    pub fn find_by_surface(&self, surface: &WlSurface) -> Option<&View> {
        self.views
            .iter()
            .find(|v| v.surface().as_ref() == Some(surface))
    }

    pub fn find_by_surface_mut(&mut self, surface: &WlSurface) -> Option<&mut View> {
        self.views
            .iter_mut()
            .find(|v| v.surface().as_ref() == Some(surface))
    }

    pub fn iter(&self) -> impl Iterator<Item = &View> {
        self.views.iter()
    }

    pub fn views_mut(&mut self) -> impl Iterator<Item = &mut View> {
        self.views.iter_mut()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_start_at_one_so_zero_stays_the_no_focus_sentinel() {
        // A window that got id 0 would be indistinguishable from "focus left
        // every client" on the view.focused message.
        let mut views = Views::new();
        assert_ne!(NO_VIEW, 1);
        assert_eq!(views.next_id, 1);
        // insert() needs a real Window, which needs a client, so the id
        // sequence is checked directly rather than through it.
        views.next_id += 1;
        assert_eq!(views.next_id, 2);
    }
}
