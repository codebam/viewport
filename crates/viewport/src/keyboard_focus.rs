// SPDX-License-Identifier: GPL-3.0-or-later
//
// What can hold the keyboard.
//
// A Wayland surface is told it has the keyboard by being sent
// `wl_keyboard.enter`. An X11 window is not: the X server has its own idea of
// which window the keyboard belongs to, and a window manager has to say so
// with `SetInputFocus`. Smithay puts that call in `KeyboardTarget for
// X11Surface`, which only ever runs if an `X11Surface` can *be* the seat's
// keyboard focus.
//
// With `type KeyboardFocus = WlSurface` it never can. The X11 window's Wayland
// surface gets the enter event, so a toolkit that only reads Wayland is happy,
// and the X server is left at `PointerRoot` — keystrokes are delivered to
// whatever window the cursor happens to be over, and no window is ever told it
// is focused.
//
// A game notices. Minecraft asks for the pointer only once it believes its
// window has focus, so under `PointerRoot` it never grabs, never asks for a
// pointer lock, and never turns the camera — while WASD still works, because
// keys follow the cursor, and clicks still work, because those follow the
// cursor too. Every symptom except the one that matters looks fine.

use smithay::input::keyboard::{KeyboardTarget, KeysymHandle, ModifiersState};
use smithay::input::Seat;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{IsAlive, Serial};
use smithay::wayland::seat::WaylandFocus;
use smithay::xwayland::X11Surface;

use crate::state::ViewportState;

/// The keyboard's focus: a Wayland surface, or an X11 window.
///
/// Both, rather than the surface alone, because an X11 window needs the X
/// server told as well — see the note at the top of this file.
// Boxed X11 arm: an `X11Surface` is far larger than a `WlSurface`, and this
// enum is stored and cloned on every focus change.
#[derive(Debug, Clone, PartialEq)]
pub enum KeyboardFocus {
    /// An ordinary Wayland client's surface: a toplevel, a layer surface, a
    /// popup, or the lock screen.
    Wayland(WlSurface),
    /// A window under Xwayland, which owns a Wayland surface too but has to be
    /// focused through the X server to count as focused.
    ///
    /// The surface is carried alongside rather than looked up on demand, so
    /// that every focus has one. Smithay's popup grabs require the keyboard
    /// focus to convert into a pointer focus infallibly, and a variant that
    /// might have no surface could not.
    X11(Box<X11Focus>),
}

/// An X11 window and the surface it draws through.
#[derive(Debug, Clone, PartialEq)]
pub struct X11Focus {
    /// The X11 window, which is what tells the X server about focus.
    pub window: X11Surface,
    /// The Wayland surface it draws through.
    pub surface: WlSurface,
}

impl KeyboardFocus {
    /// Focus for an X11 window, if it has a surface to focus yet.
    ///
    /// A window that has been created but has not associated a surface has
    /// nothing to give the keyboard to; the caller leaves focus where it is.
    pub fn x11(window: &X11Surface) -> Option<Self> {
        window.wl_surface().map(|surface| {
            KeyboardFocus::X11(Box::new(X11Focus {
                window: window.clone(),
                surface,
            }))
        })
    }

    /// Focus for a window, of whichever kind it turns out to be.
    ///
    /// The distinction is not cosmetic: `Window::toplevel()` is `None` for an
    /// X11 window, so a call site that reaches for the toplevel's surface
    /// focuses nothing at all when the window came from Xwayland.
    pub fn for_window(window: &smithay::desktop::Window) -> Option<Self> {
        if let Some(x11) = window.x11_surface() {
            return Self::x11(x11);
        }
        window
            .toplevel()
            .map(|toplevel| KeyboardFocus::Wayland(toplevel.wl_surface().clone()))
    }

    /// The Wayland surface behind this focus, whichever kind it is.
    pub fn surface(&self) -> &WlSurface {
        match self {
            KeyboardFocus::Wayland(surface) => surface,
            KeyboardFocus::X11(x11) => &x11.surface,
        }
    }

    /// Whether this focus is that surface, whichever kind it is.
    ///
    /// Call sites compare against a `WlSurface` because that is what the rest
    /// of the compositor tracks; an X11 focus answers for its own surface.
    pub fn is_surface(&self, other: &WlSurface) -> bool {
        self.surface() == other
    }
}

impl From<WlSurface> for KeyboardFocus {
    fn from(surface: WlSurface) -> Self {
        KeyboardFocus::Wayland(surface)
    }
}

/// Required by smithay's popup grabs, which turn the keyboard focus into a
/// pointer focus. Total, which is why the X11 variant carries its surface.
impl From<KeyboardFocus> for WlSurface {
    fn from(focus: KeyboardFocus) -> Self {
        match focus {
            KeyboardFocus::Wayland(surface) => surface,
            KeyboardFocus::X11(x11) => x11.surface,
        }
    }
}

/// Required by `PopupKeyboardGrab`, which hands focus to a popup by kind.
impl From<smithay::desktop::PopupKind> for KeyboardFocus {
    fn from(popup: smithay::desktop::PopupKind) -> Self {
        KeyboardFocus::Wayland(popup.wl_surface().clone())
    }
}

impl IsAlive for KeyboardFocus {
    fn alive(&self) -> bool {
        match self {
            KeyboardFocus::Wayland(surface) => surface.alive(),
            KeyboardFocus::X11(x11) => x11.window.alive(),
        }
    }
}

impl WaylandFocus for KeyboardFocus {
    fn wl_surface(&self) -> Option<std::borrow::Cow<'_, WlSurface>> {
        match self {
            KeyboardFocus::Wayland(surface) => Some(std::borrow::Cow::Borrowed(surface)),
            KeyboardFocus::X11(x11) => Some(std::borrow::Cow::Borrowed(&x11.surface)),
        }
    }
}

/// Pure delegation. The point of the enum is which arm is taken, not what
/// either arm does: `X11Surface`'s own implementation is the one that calls
/// `SetInputFocus`, and `WlSurface`'s is the one that sends `wl_keyboard`
/// events. Both are smithay's.
impl KeyboardTarget<ViewportState> for KeyboardFocus {
    fn enter(
        &self,
        seat: &Seat<ViewportState>,
        data: &mut ViewportState,
        keys: Vec<KeysymHandle<'_>>,
        serial: Serial,
    ) {
        match self {
            KeyboardFocus::Wayland(surface) => {
                KeyboardTarget::enter(surface, seat, data, keys, serial)
            }
            KeyboardFocus::X11(x11) => KeyboardTarget::enter(&x11.window, seat, data, keys, serial),
        }
    }

    fn leave(&self, seat: &Seat<ViewportState>, data: &mut ViewportState, serial: Serial) {
        match self {
            KeyboardFocus::Wayland(surface) => KeyboardTarget::leave(surface, seat, data, serial),
            KeyboardFocus::X11(x11) => KeyboardTarget::leave(&x11.window, seat, data, serial),
        }
    }

    fn key(
        &self,
        seat: &Seat<ViewportState>,
        data: &mut ViewportState,
        key: KeysymHandle<'_>,
        state: smithay::backend::input::KeyState,
        serial: Serial,
        time: u32,
    ) {
        match self {
            KeyboardFocus::Wayland(surface) => {
                KeyboardTarget::key(surface, seat, data, key, state, serial, time)
            }
            KeyboardFocus::X11(x11) => {
                KeyboardTarget::key(&x11.window, seat, data, key, state, serial, time)
            }
        }
    }

    fn modifiers(
        &self,
        seat: &Seat<ViewportState>,
        data: &mut ViewportState,
        modifiers: ModifiersState,
        serial: Serial,
    ) {
        match self {
            KeyboardFocus::Wayland(surface) => {
                KeyboardTarget::modifiers(surface, seat, data, modifiers, serial)
            }
            KeyboardFocus::X11(x11) => {
                KeyboardTarget::modifiers(&x11.window, seat, data, modifiers, serial)
            }
        }
    }
}
