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
    /// The frame the shell drew, when it has to be drawn above the windows
    /// underneath this one — see `ViewLayout::frame`.
    pub frame: Option<Box>,
    /// Element ids for that frame, one per side, stable for the life of the
    /// view: the damage tracker keys on the id, and one that changed every
    /// frame would redraw the whole output for a border.
    ///
    /// Four, because what is drawn is the border and not the window. The
    /// middle of a frame is the desktop's own background in the shell's buffer
    /// — `.viewport` has no background, but the wallpaper behind it does — so
    /// drawing the whole rectangle paints the desktop over the client, which is
    /// a floating window that has become a solid block of wallpaper.
    pub overlay_ids: [smithay::backend::renderer::element::Id; 4],

    /// Driven a frame at a time by a tween in the shell, which cannot fade a
    /// window with CSS: the frame is DOM, the contents are a surface the
    /// compositor draws.
    pub opacity: f32,

    /// The last surface size that did not match the rectangle it was given,
    /// so the mismatch is said once rather than per frame.
    pub last_mismatch: Option<(i32, i32)>,

    /// This window's entry in the foreign toplevel list, so anything outside
    /// the compositor can see it. Absent until the window is announced: a
    /// window with no title and no app id is not worth listing.
    pub foreign: Option<smithay::wayland::foreign_toplevel_list::ForeignToplevelHandle>,

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

    /// The surface this window paints into.
    ///
    /// An X11 window has no xdg toplevel — its surface arrives later, when
    /// Xwayland associates the X11 window with a wl_surface — so reading only
    /// the toplevel means an X11 window is never found by surface. Everything
    /// that announces a window to the shell starts with that lookup, so Steam
    /// and every other X11 client mapped, painted, and were never placed:
    /// present in the window list, invisible on screen.
    pub fn surface(&self) -> Option<WlSurface> {
        if let Some(toplevel) = self.window.toplevel() {
            return Some(toplevel.wl_surface().clone());
        }
        self.window
            .x11_surface()
            .and_then(|x11| x11.wl_surface())
            .map(|surface| surface.clone())
    }

    pub fn title(&self) -> String {
        // An X11 window keeps its name in a property rather than in the xdg
        // role attributes, so a window with no title in the dock is what
        // reading only the latter gives.
        if let Some(x11) = self.window.x11_surface() {
            return x11.title();
        }
        // Two layers of Option: the surface may have no role attributes, and
        // the client may not have set a title.
        self.role_attribute(|attrs| attrs.title.clone())
            .flatten()
            .unwrap_or_default()
    }

    pub fn app_id(&self) -> String {
        // WM_CLASS is the X11 equivalent, and it is what a window rule and an
        // icon lookup both key on.
        if let Some(x11) = self.window.x11_surface() {
            return x11.class();
        }
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
    /// The compositor can see these signals and the shell cannot, which is why
    /// the answer is sent with the window rather than worked out there. A
    /// window rule in the config still overrides it: this is what the
    /// application asked for, not what the user settled on.
    pub fn wants_floating(&self) -> bool {
        // An X11 window says so outright, in the type it advertises. Reading
        // it is what the C build does, and without it every GIMP tool palette
        // and every splash screen is tiled into the layout as though it were a
        // main window.
        if let Some(x11) = self.window.x11_surface() {
            use smithay::xwayland::xwm::WmWindowType;
            if let Some(kind) = x11.window_type() {
                if matches!(
                    kind,
                    WmWindowType::Dialog
                        | WmWindowType::Utility
                        | WmWindowType::Toolbar
                        | WmWindowType::Splash
                        | WmWindowType::Menu
                        | WmWindowType::DropdownMenu
                        | WmWindowType::PopupMenu
                        | WmWindowType::Notification
                        | WmWindowType::Tooltip
                        | WmWindowType::Combo
                        | WmWindowType::Dnd
                ) {
                    return true;
                }
            }
            // A window opened on behalf of another one — the X11 spelling of a
            // parent, and what a dialog with no type set still has.
            if x11.is_transient_for().is_some() {
                return true;
            }
            // Never placed by the compositor at all, in the protocol's own
            // terms: tooltips and menus that position themselves.
            if x11.is_override_redirect() {
                return true;
            }
        }

        let Some(toplevel) = self.window.toplevel() else {
            return false;
        };

        // A parent toplevel means a dialog.
        if toplevel.parent().is_some() {
            return true;
        }

        // xdg-dialog-v1, where a client says it plainly rather than leaving it
        // to be inferred. Modal counts the same as dialog here: both are
        // windows that came up to be dealt with and go away, and neither
        // belongs in a tiling column.
        use smithay::wayland::shell::xdg::dialog::ToplevelDialogHint;
        if self
            .role_attribute(|attrs| attrs.dialog_hint)
            .is_some_and(|hint| matches!(hint, ToplevelDialogHint::Dialog | ToplevelDialogHint::Modal))
        {
            return true;
        }

        // A window that cannot be resized. Tiling it means either breaking the
        // layout or asking a client to do something it has said it will not,
        // and what a client means by a fixed size is almost always a dialog:
        // an about box, a preferences panel, a password prompt.
        let (min, max) = self.size_bounds();
        if min.0 > 0 && min.1 > 0 && min == max {
            return true;
        }

        false
    }

    /// What the client will accept, as (minimum, maximum). Zero for "no
    /// opinion", which is what the protocol uses.
    fn size_bounds(&self) -> ((i32, i32), (i32, i32)) {
        let Some(surface) = self.surface() else {
            return ((0, 0), (0, 0));
        };
        with_states(&surface, |states| {
            let mut guard = states.cached_state.get::<SurfaceCachedState>();
            let current = guard.current();
            (
                (current.min_size.w, current.min_size.h),
                (current.max_size.w, current.max_size.h),
            )
        })
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
            last_mismatch: None,
            id,
            window,
            mapped: false,
            placed: false,
            box_: Box::new(0, 0, 0, 0),
            visible: true,
            scale: 1.0,
            clip: None,
            frame: None,
            overlay_ids: std::array::from_fn(|_| {
                smithay::backend::renderer::element::Id::new()
            }),
            opacity: 1.0,
            configured: None,
            foreign: None,
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
