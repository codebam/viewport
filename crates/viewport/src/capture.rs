// SPDX-License-Identifier: GPL-3.0-or-later
//
// Keeping the keys when nested, the way a virtual machine does.
//
// A compositor inside another compositor competes with it for every chord. The
// host sees Mod4+Return first and opens *its* terminal; the nested session
// gets whatever the host did not want. Testing a keybinding then means either
// picking one the host does not use or switching to a TTY, and both of those
// are ways of not testing the thing you meant to test.
//
// `zwp_keyboard_shortcuts_inhibit_v1` is the protocol for exactly this. A
// client asks the host to stop acting on its own shortcuts while a particular
// surface has the keyboard, and the host either agrees or does not. Viewport
// implements the *server* half already — see
// `ViewportState::shortcuts_inhibited` — so a nested Viewport asking a
// Viewport host works; a nested Viewport under anything else works as far as
// that compositor supports the protocol, and quietly does not where it does
// not.
//
// The host suppresses its shortcuts only while the inhibiting surface holds
// the keyboard, so clicking another window already hands them back. What that
// does not cover is wanting the host's chords *while* the nested window is
// where you are working, which is what a virtual machine's ungrab key is for —
// so Ctrl+Alt+G releases and takes it back, the chord QEMU and virt-manager
// use, chosen because it is the one already in people's hands.
//
// This is the only place this compositor is a Wayland *client*. winit owns the
// connection and does not expose the protocol, so the objects here ride on the
// same `wl_display` through a second event queue: `Backend::from_foreign_display`
// hands the pointer back to wayland-client without taking ownership of it, and
// dropping everything here leaves winit's connection untouched.

use std::os::raw::c_void;

use wayland_client::protocol::{wl_registry, wl_seat, wl_surface};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::{
    zwp_keyboard_shortcuts_inhibit_manager_v1 as manager, zwp_keyboard_shortcuts_inhibitor_v1,
};

/// The inhibitor, and everything holding it up.
///
/// Kept whole because every object in it has to outlive the request: dropping
/// the inhibitor destroys it, and the host hands its shortcuts back.
pub struct Capture {
    connection: Connection,
    queue: wayland_client::EventQueue<Globals>,
    manager: manager::ZwpKeyboardShortcutsInhibitManagerV1,
    seat: wl_seat::WlSeat,
    surface: wl_surface::WlSurface,
    /// The inhibitor, while there is one. `None` after Ctrl+Alt+G, which is
    /// the whole of what "released" means: destroying it is what tells the
    /// host it may act on its own chords again.
    inhibitor: Option<zwp_keyboard_shortcuts_inhibitor_v1::ZwpKeyboardShortcutsInhibitorV1>,
}

impl Capture {
    /// Whether the keys are currently ours.
    pub fn held(&self) -> bool {
        self.inhibitor.is_some()
    }

    /// Take the keyboard, or give it back. Returns what it is now.
    pub fn toggle(&mut self) -> bool {
        let mut globals = Globals::default();
        match self.inhibitor.take() {
            Some(inhibitor) => {
                inhibitor.destroy();
                tracing::info!("keyboard released to the host; Ctrl+Alt+G takes it back");
            }
            None => {
                let handle = self.queue.handle();
                self.inhibitor =
                    Some(
                        self.manager
                            .inhibit_shortcuts(&self.surface, &self.seat, &handle, ()),
                    );
                tracing::info!("keyboard captured; Ctrl+Alt+G releases it");
            }
        }
        // Or the request sits in the outgoing buffer until something else
        // happens to flush it, and a release does not take effect until the
        // next key — which reads as the chord not working.
        let _ = self.queue.roundtrip(&mut globals);
        let _ = self.connection.flush();
        self.held()
    }
}

#[derive(Default)]
struct Globals {
    manager: Option<manager::ZwpKeyboardShortcutsInhibitManagerV1>,
    seat: Option<wl_seat::WlSeat>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for Globals {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        handle: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name, interface, ..
        } = event
        else {
            return;
        };
        match interface.as_str() {
            "zwp_keyboard_shortcuts_inhibit_manager_v1" => {
                state.manager = Some(registry.bind(name, 1, handle, ()));
            }
            // Any seat will do: this compositor has one, and so does every
            // host it is plausibly nested inside. The first is taken rather
            // than the one named, because a nested session has no way to know
            // which seat its window is being typed into.
            "wl_seat" if state.seat.is_none() => {
                state.seat = Some(registry.bind(name, 1, handle, ()));
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for Globals {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<manager::ZwpKeyboardShortcutsInhibitManagerV1, ()> for Globals {
    fn event(
        _: &mut Self,
        _: &manager::ZwpKeyboardShortcutsInhibitManagerV1,
        _: manager::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwp_keyboard_shortcuts_inhibitor_v1::ZwpKeyboardShortcutsInhibitorV1, ()>
    for Globals
{
    fn event(
        _: &mut Self,
        _: &zwp_keyboard_shortcuts_inhibitor_v1::ZwpKeyboardShortcutsInhibitorV1,
        event: zwp_keyboard_shortcuts_inhibitor_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Reported rather than acted on. Which state it is in is the host's
        // business, and the only thing this end can do about "inactive" is
        // say so — a host that declines has declined.
        match event {
            zwp_keyboard_shortcuts_inhibitor_v1::Event::Active => {
                tracing::info!("the host is holding its shortcuts back; keys are ours");
            }
            zwp_keyboard_shortcuts_inhibitor_v1::Event::Inactive => {
                tracing::info!("the host has taken its shortcuts back");
            }
            _ => {}
        }
    }
}

/// Ask the host to stop acting on its own shortcuts while this window has the
/// keyboard.
///
/// # Safety
///
/// `display` and `surface` must be the `wl_display` and `wl_surface` pointers
/// of a live window — winit's, in practice, which owns both for as long as the
/// backend does.
pub unsafe fn keep_the_keys(display: *mut c_void, surface: *mut c_void) -> anyhow::Result<Capture> {
    use wayland_client::backend::Backend;

    // Borrowed, not adopted: `from_foreign_display` does not take ownership,
    // so winit goes on driving its own queue and this one only ever sends the
    // handful of requests below.
    let backend = unsafe { Backend::from_foreign_display(display.cast()) };
    let connection = Connection::from_backend(backend);
    let mut queue = connection.new_event_queue();
    let handle = queue.handle();

    let mut globals = Globals::default();
    let _registry = connection.display().get_registry(&handle, ());
    // Twice: the first pass delivers the globals, and the binds it makes are
    // only sent on the second.
    queue.roundtrip(&mut globals)?;
    queue.roundtrip(&mut globals)?;

    let Some(manager) = globals.manager.clone() else {
        anyhow::bail!("the host does not offer zwp_keyboard_shortcuts_inhibit_manager_v1");
    };
    let Some(seat) = globals.seat.clone() else {
        anyhow::bail!("the host offered no wl_seat");
    };

    // The surface belongs to winit and this only names it. `from_id` is the
    // way to speak about an object this queue did not create.
    let id = unsafe {
        wayland_client::backend::ObjectId::from_ptr(
            wl_surface::WlSurface::interface(),
            surface.cast(),
        )
    }?;
    let surface = wl_surface::WlSurface::from_id(&connection, id)?;

    let inhibitor = manager.inhibit_shortcuts(&surface, &seat, &handle, ());
    queue.roundtrip(&mut globals)?;

    Ok(Capture {
        connection,
        queue,
        manager,
        seat,
        surface,
        inhibitor: Some(inhibitor),
    })
}
