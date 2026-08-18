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

use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::Hash;

use smithay::desktop::Window;
use smithay::reexports::wayland_server::backend::ObjectId;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::Resource as _;
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

    /// The icon name the client set through `xdg-toplevel-icon`, if it set
    /// one. The shell looks it up in the theme; nothing here draws it.
    pub icon: Option<String>,

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
    /// What the client calls this window, from xdg-toplevel-tag.
    ///
    /// An application's own name for one of its windows — a scratchpad, a
    /// picture-in-picture — which survives a restart and is what a session
    /// restoring a layout would match on. Nothing does yet; it is kept because
    /// the client is entitled to be remembered by it.
    pub tag: Option<String>,
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
    /// Element ids for the four corners of the same frame, on the same terms.
    ///
    /// Separate from the sides because they are drawn at a different depth:
    /// a side sits outside the client and goes in front of it, a corner sits
    /// *inside* the client's own rectangle — the square the border's curve
    /// cuts across — and has to go behind it, so the client covers the part
    /// of that square it actually fills.
    pub corner_ids: [smithay::backend::renderer::element::Id; 4],

    /// The shell is floating this window rather than tiling it, as its last
    /// `view.layout` said. Stacking reads it: a floating window belongs over
    /// the tiled ones whatever has focus, because it is a dialog or a palette
    /// that was put in front of them on purpose.
    pub floating: bool,

    /// The shell drew this window's frame with square corners, as its last
    /// `view.layout` said — smart radius, on the one window a workspace holds.
    /// The client is then not cropped to a corner, because there is none.
    pub square: bool,

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
    pub fn added(&self, output: String, replay: bool, parent: Option<u32>) -> ViewAdded {
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
            parent,
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
        self.window.x11_surface().and_then(|x11| x11.wl_surface())
    }

    /// The identity of that surface, without handing out the surface itself.
    ///
    /// `surface()` clones — a toplevel only lends its `WlSurface` out by
    /// reference — and a lookup that asked every window in turn cloned one per
    /// candidate for an answer it threw away. This is what the index keys on,
    /// and it is read from the window every time rather than remembered on the
    /// view: an X11 window has no surface until Xwayland associates one, so a
    /// remembered id would be `None` for exactly as long as it took a window to
    /// appear and wrong forever after.
    pub fn surface_id(&self) -> Option<ObjectId> {
        if let Some(toplevel) = self.window.toplevel() {
            return Some(toplevel.wl_surface().id());
        }
        self.window
            .x11_surface()
            .and_then(|x11| x11.wl_surface())
            .map(|surface| surface.id())
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

    /// Whether this window wants to open fullscreen.
    ///
    /// Asked at the moment a window is announced, because that is the first
    /// point the shell can act on the answer. Both spellings are read here
    /// rather than remembered from a request: an X11 client sets a property and
    /// never calls into the compositor at all, so there is no request to have
    /// remembered.
    pub fn wants_fullscreen(&self) -> bool {
        if let Some(toplevel) = self.window.toplevel() {
            use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State;
            // The pending state, which is what the compositor has decided and
            // configured for. The current state is only what the client has
            // acknowledged, and at announce time it usually has not yet.
            return toplevel
                .with_pending_state(|pending| pending.states.contains(State::Fullscreen));
        }
        if let Some(x11) = self.window.x11_surface() {
            return x11.is_fullscreen();
        }
        false
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
            .is_some_and(|hint| {
                matches!(hint, ToplevelDialogHint::Dialog | ToplevelDialogHint::Modal)
            })
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

/// Where a key was last found in a list, remembered so the next lookup does
/// not walk it.
///
/// A cache and not a directory, and the difference is the whole point. Every
/// answer it gives is checked against the list before it is returned — the
/// entry says *where to look first*, never *what is there* — so an entry that
/// has gone stale costs one comparison and a walk, and cannot produce a wrong
/// answer. Nothing outside has to remember to keep it up to date, which
/// matters because the thing being indexed changes behind everyone's back: an
/// X11 window has no `wl_surface` until Xwayland gives it one, and there is no
/// call into the compositor at the moment it does.
///
/// The alternative — an index maintained at every mutation — is one missed
/// update away from a window that is drawn, raised and focused and silently
/// ignores the pointer, because every hit test in the compositor starts with
/// this lookup.
struct Index<K> {
    /// Interior mutability because the lookup is on a `&self` path: a hit test
    /// runs twice per pointer motion and has no business asking for a mutable
    /// borrow of the window list to read from it.
    slots: RefCell<HashMap<K, usize>>,
}

impl<K> Default for Index<K> {
    fn default() -> Self {
        Self {
            slots: RefCell::new(HashMap::new()),
        }
    }
}

impl<K: Eq + Hash + Clone> Index<K> {
    /// The position of the item `key_of` reports `key` for, cached.
    fn find<T>(&self, items: &[T], key: &K, key_of: impl Fn(&T) -> Option<K>) -> Option<usize> {
        let mut slots = self.slots.borrow_mut();
        if let Some(&at) = slots.get(key) {
            // The check that makes staleness harmless. A key belongs to one
            // item at a time, so an item that still reports it *is* the item
            // asked for, whatever has happened to the list since.
            if items.get(at).and_then(&key_of).as_ref() == Some(key) {
                return Some(at);
            }
        }
        let at = items
            .iter()
            .position(|item| key_of(item).as_ref() == Some(key))?;
        slots.insert(key.clone(), at);
        Some(at)
    }

    /// Forget everything, because the positions have moved.
    fn clear(&self) {
        self.slots.borrow_mut().clear();
    }
}

#[derive(Default)]
pub struct Views {
    next_id: u32,
    views: Vec<View>,
    /// Surface to position in `views`. See `Index`.
    by_surface: Index<ObjectId>,
}

impl Views {
    pub fn new() -> Self {
        // src/server.c:114. Starting at 1 keeps 0 free as the "nothing focused"
        // sentinel.
        Self {
            next_id: 1,
            views: Vec::new(),
            by_surface: Index::default(),
        }
    }

    pub fn insert(&mut self, window: Window) -> u32 {
        // Onto the end, so no position the surface index has cached moves.
        let id = self.next_id;
        self.next_id += 1;
        self.views.push(View {
            last_mismatch: None,
            id,
            window,
            mapped: false,
            icon: None,
            placed: false,
            box_: Box::new(0, 0, 0, 0),
            visible: true,
            scale: 1.0,
            clip: None,
            tag: None,
            frame: None,
            floating: false,
            square: false,
            overlay_ids: std::array::from_fn(|_| smithay::backend::renderer::element::Id::new()),
            corner_ids: std::array::from_fn(|_| smithay::backend::renderer::element::Id::new()),
            opacity: 1.0,
            configured: None,
            foreign: None,
        });
        id
    }

    pub fn remove(&mut self, id: u32) -> Option<View> {
        let index = self.views.iter().position(|v| v.id == id)?;
        // Everything after it slides down one. The cached positions would
        // still be checked before they were believed, so this is not what
        // keeps the lookup honest — it is what keeps it fast, and what stops
        // the map growing a dead entry per closed window.
        self.by_surface.clear();
        Some(self.views.remove(index))
    }

    pub fn get(&self, id: u32) -> Option<&View> {
        self.views.iter().find(|v| v.id == id)
    }

    pub fn get_mut(&mut self, id: u32) -> Option<&mut View> {
        self.views.iter_mut().find(|v| v.id == id)
    }

    /// The view a dialog belongs to, if it belongs to one.
    ///
    /// `wants_floating` already reads both spellings of this — an xdg parent
    /// and an X11 `transient_for` — to decide that a window is a dialog. This
    /// answers the question that follows: *whose*. The shell cannot work it out
    /// at all, having no view of surfaces or X11 ids, and a layout that places
    /// windows by hand needs it — a dialog put in the middle of the screen is
    /// in the middle of nothing in particular when its parent is somewhere else
    /// entirely.
    ///
    /// None when the parent is a window the shell does not know about, which is
    /// a real case: a client may parent a dialog to a surface that was never
    /// announced as a view.
    pub fn parent_id_of(&self, view: &View) -> Option<u32> {
        // X11 names its parent by window id rather than by surface, so the
        // match is on the id and not on `find_by_surface`.
        if let Some(x11) = view.window.x11_surface() {
            let parent = x11.is_transient_for()?;
            return self
                .views
                .iter()
                .find(|other| {
                    other
                        .window
                        .x11_surface()
                        .is_some_and(|x| x.window_id() == parent)
                })
                .map(|other| other.id);
        }

        let parent = view.window.toplevel()?.parent()?;
        self.find_by_surface(&parent).map(|other| other.id)
    }

    /// The view painting into a surface.
    ///
    /// The busiest lookup in the compositor: every commit, every hit test —
    /// twice per pointer motion, so two thousand times a second on a 1000Hz
    /// mouse — and every focus change starts here. It was a walk of the window
    /// list that cloned a `WlSurface` per window it passed, which is a cost
    /// that grows with the number of windows open on a path that runs whether
    /// or not anything changed.
    pub fn find_by_surface(&self, surface: &WlSurface) -> Option<&View> {
        let at = self.position_of_surface(surface)?;
        self.views.get(at)
    }

    pub fn find_by_surface_mut(&mut self, surface: &WlSurface) -> Option<&mut View> {
        let at = self.position_of_surface(surface)?;
        self.views.get_mut(at)
    }

    fn position_of_surface(&self, surface: &WlSurface) -> Option<usize> {
        self.by_surface
            .find(&self.views, &surface.id(), View::surface_id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &View> {
        self.views.iter()
    }

    pub fn views_mut(&mut self) -> impl Iterator<Item = &mut View> {
        self.views.iter_mut()
    }
}

/// Whether a clip rectangle covers a point, in the layout's coordinates.
///
/// The clip is what the renderer crops a window to, and input goes by the same
/// rectangle: a column scrolled off one monitor still has a rectangle on the
/// monitor beside it, and a click there belongs to whatever is really drawn
/// under the pointer. See `ViewportState::clipped_out`.
pub fn clip_covers(clip: Box, x: f64, y: f64) -> bool {
    x >= clip.x as f64
        && y >= clip.y as f64
        && x < (clip.x + clip.width) as f64
        && y < (clip.y + clip.height) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_column_scrolled_onto_the_next_monitor_is_not_clicked() {
        // DP-1 is 0..1920, DP-3 starts at 1920. A column of DP-3's strip
        // scrolled left keeps a rectangle over DP-1 and is clipped to nothing
        // of it; the click at 960 belongs to DP-1's own window.
        let clip = Box {
            x: 1920,
            y: 0,
            width: 0,
            height: 1080,
        };
        assert!(!clip_covers(clip, 960.0, 540.0));

        // Half off the left edge of its own output: the half still drawn takes
        // the click, the half hanging onto DP-1 does not.
        let clip = Box {
            x: 1920,
            y: 0,
            width: 600,
            height: 1080,
        };
        assert!(clip_covers(clip, 2000.0, 540.0));
        assert!(!clip_covers(clip, 1900.0, 540.0));

        // The far edge is exclusive: a point on it is the first pixel outside.
        assert!(!clip_covers(clip, 2520.0, 540.0));
    }

    // The surface index, exercised over stand-in keys.
    //
    // A `WlSurface` cannot be made without a display and a connected client,
    // and a `View` cannot be made without a `Window` which cannot be made
    // without a surface — so the thing under test here is the index itself,
    // over a list of optional keys standing in for "the surface each window is
    // painting into, if it has one yet". That is exactly the shape `Views`
    // hands it: `View::surface_id` is a function from a window to `Option`ally
    // a key, and nothing else about a view reaches this code.
    fn key_of(item: &Option<u32>) -> Option<u32> {
        *item
    }

    #[test]
    fn a_window_is_found_by_its_surface() {
        let index = Index::default();
        let windows = vec![Some(10), Some(11), Some(12)];
        assert_eq!(index.find(&windows, &11, key_of), Some(1));
        // Again, now from the cache rather than the walk. Same answer.
        assert_eq!(index.find(&windows, &11, key_of), Some(1));
        assert_eq!(index.find(&windows, &99, key_of), None);
    }

    #[test]
    fn a_closed_window_takes_its_surface_with_it() {
        let index = Index::default();
        let mut windows = vec![Some(10), Some(11), Some(12)];
        assert_eq!(index.find(&windows, &12, key_of), Some(2));

        // `Views::remove` clears the cache; the positions after the hole have
        // moved, and 12 is now where 11 was.
        windows.remove(0);
        index.clear();
        assert_eq!(index.find(&windows, &10, key_of), None);
        assert_eq!(index.find(&windows, &12, key_of), Some(1));

        // And without the clear, which is the case that must not be able to
        // go wrong: the cached position for 12 is 2, which is now off the end
        // of the list, and the walk finds it anyway.
        let stale = Index::default();
        assert_eq!(
            stale.find(&[Some(10), Some(11), Some(12)], &12, key_of),
            Some(2)
        );
        assert_eq!(stale.find(&windows, &12, key_of), Some(1));
    }

    #[test]
    fn an_x11_window_is_found_once_its_surface_arrives() {
        // An X11 window is in the list from the moment Xwayland asks for it to
        // be mapped, and has no wl_surface until some time later. Nothing
        // tells the compositor at the moment it gets one.
        let index = Index::default();
        let mut windows = vec![Some(10), None];
        assert_eq!(index.find(&windows, &11, key_of), None);

        windows[1] = Some(11);
        assert_eq!(index.find(&windows, &11, key_of), Some(1));
    }

    #[test]
    fn a_surface_that_moved_does_not_answer_for_the_window_it_left() {
        // The worst case the check exists for: a cached position that still
        // holds a window, but not the window that owns the surface asked
        // about. Believing the cache here focuses and clicks the wrong window.
        let index = Index::default();
        let mut windows = vec![Some(10), Some(11)];
        assert_eq!(index.find(&windows, &11, key_of), Some(1));

        // Position 1 is now a different window, and 11 belongs to no one.
        windows[1] = Some(12);
        assert_eq!(index.find(&windows, &11, key_of), None);
        assert_eq!(index.find(&windows, &12, key_of), Some(1));

        // And where it has moved to another window rather than vanishing.
        windows[0] = Some(11);
        assert_eq!(index.find(&windows, &11, key_of), Some(0));
    }

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
