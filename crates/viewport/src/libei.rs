// SPDX-License-Identifier: GPL-3.0-or-later
//
// The EI server, which is how a remote-desktop session drives this machine
// when it is not doing it one D-Bus call at a time.
//
// Version 2 of org.freedesktop.impl.portal.RemoteDesktop added one method,
// ConnectToEIS, and it exists because the eleven Notify calls beside it are a
// round trip through the portal frontend per input event. A remote pointer
// moves hundreds of times a second; a remote session typing at speed sends two
// messages a keystroke. Every one of them is a message the bus thread has to
// receive, check against a grant and forward over a channel. libei replaces
// the whole of that with a socket the application speaks to directly, and the
// events arrive here already batched into frames.
//
// The socket is a `socketpair`. The bus thread makes the pair and hands one
// half to the application as the file descriptor ConnectToEIS answers with —
// see `screencast::remote::connect_to_eis` — and sends the other half here.
// That split is not an arrangement of convenience: neither thread can do the
// other's job. The bus thread cannot build the `eis::Context`, because a
// context is read by a calloop source that owns it, calloop is the
// compositor's loop, and `ViewportState` is not `Send`. The compositor thread
// cannot answer the D-Bus call, because that is a synchronous reply on the
// connection the settings portal shares and it has to carry an fd back
// immediately. So the pair is made where the answer is needed and the reading
// end travels over the channel that already carries every other portal
// message, exactly as `Message::Inject` does.
//
// Consent is enforced when the connection is set up rather than per event,
// which is the opposite of the Notify path and the stronger of the two. There,
// every call names a session and is checked against what that session was
// granted, because a D-Bus method is reachable whether or not it should be.
// Here the grant decides which devices exist at all: a session allowed a mouse
// and not a keyboard is given a seat with no keyboard on it, so there is no
// object for the client to bind and no keystroke it can compose. That is read
// from `granted_devices` and never from `wanted_devices`, which is what the
// application asked for and is not a permission.
//
// Revocation is the other half of that, and it is the half the Notify path
// gets for free: there, closing a session takes the row out of the table and
// the next event is dropped. Here the client is already holding a socket, and
// a socket outlives a table. So a live connection is remembered by session
// handle, and the session ending — the frontend calling Close, or the
// frontend's own bus name going away — drops the calloop source, which drops
// the context, which closes this end of the socket. The client reads
// end-of-file and stops. It is the only teardown that is not a request the
// client could decline.

use std::collections::HashMap;
use std::os::unix::net::UnixStream;

use smithay::backend::input::{
    AbsolutePositionEvent as _, Event as _, InputEvent, KeyState, KeyboardKeyEvent as _,
    PointerButtonEvent as _, TouchEvent as _,
};
use smithay::backend::libei::{EiInput, EiInputConnection, EiInputEvent, EiInputSeat, EiRegion};
use smithay::input::keyboard::{Keycode, Keysym, XkbConfig};
use smithay::reexports::calloop::RegistrationToken;
use smithay::reexports::reis::eis;
use zvariant::OwnedObjectPath;

use crate::screencast::remote::{DEVICE_KEYBOARD, DEVICE_POINTER, DEVICE_TOUCHSCREEN};
use crate::state::ViewportState;

/// What the seat and its devices are called.
///
/// The client shows these to whoever is driving — a remote-support tool lists
/// the devices it has bound — so they name this compositor rather than the
/// protocol. Fixed strings rather than anything the application supplies: an
/// application that could name its own devices could name them after the
/// user's own hardware.
const SEAT_NAME: &str = "viewport";
const KEYBOARD_NAME: &str = "viewport remote keyboard";
const POINTER_NAME: &str = "viewport remote pointer";
const ABSOLUTE_NAME: &str = "viewport remote pointer (absolute)";
const TOUCH_NAME: &str = "viewport remote touchscreen";
const TEXT_NAME: &str = "viewport remote text";

/// Every libei connection this compositor is serving, by the portal session
/// that is allowed to have it.
///
/// Keyed by session handle rather than kept as a list, because both things
/// ever done to one are done by name: the frontend closes a session, and this
/// is what says which socket that was. One connection per session — a second
/// ConnectToEIS on the same handle replaces the first, for the reason given on
/// [`ViewportState::connect_eis`].
#[derive(Default)]
pub struct Connections {
    live: HashMap<OwnedObjectPath, Connection>,
    /// What every connected client was last told the modifiers are.
    ///
    /// One value for all of them, because there is one seat and they are all
    /// being told about it. Kept so that ordinary typing — which changes the
    /// modifier state twice per shifted character — does not put two messages
    /// on every remote socket per keystroke when nothing a client cares about
    /// has changed. A client that connects later is caught up by
    /// [`ViewportState::prime_eis_modifiers`] rather than by this.
    modifiers: Option<smithay::input::keyboard::SerializedMods>,
}

impl Connections {
    /// Whether this modifier state is news, and remember it if it is.
    ///
    /// Split out from the send so the rule can be tested without a socket:
    /// ordinary typing moves the modifier state twice per shifted character
    /// and almost none of that is a change any client has to hear about.
    fn changed(&mut self, mods: smithay::input::keyboard::SerializedMods) -> bool {
        if self.modifiers == Some(mods) {
            return false;
        }
        self.modifiers = Some(mods);
        true
    }
}

/// The four numbers `ei_keyboard.modifiers` takes, in the order it takes them.
///
/// Written once because the order is a trap: `wl_keyboard.modifiers` is
/// depressed, latched, locked, group, and libei's is depressed, **locked,
/// latched**, group. Both are bitmasks over the same modifiers, so swapping
/// the middle pair compiles, runs, and shows up only as a held Shift that a
/// remote client reads as a latched one.
fn modifier_args(mods: smithay::input::keyboard::SerializedMods) -> (u32, u32, u32, u32) {
    (
        mods.depressed,
        mods.locked,
        mods.latched,
        mods.layout_effective,
    )
}

/// One libei client.
struct Connection {
    /// What the calloop source is called, so it can be dropped again. Dropping
    /// it is the revocation; see the note at the top of this file.
    token: RegistrationToken,
    /// What the person at the machine allowed this session, as the portal's
    /// bitmask.
    ///
    /// Kept on the connection rather than looked up when the client finishes
    /// its handshake: the session's row lives on the bus thread's side of the
    /// channel behind a lock, and reading it from here would be this thread
    /// asking that one a question it has already answered. The answer cannot
    /// go stale either — a grant is only ever revoked by closing the session,
    /// and closing the session takes this whole connection with it.
    devices: u32,
    /// The seat, once the client has finished its handshake and been given
    /// one. Kept so its regions can be refreshed when the monitors move: an
    /// absolute pointer is told the layout's own coordinates, and a layout
    /// that changed without the client hearing about it is a remote pointer
    /// landing on the wrong monitor.
    seat: Option<EiInputSeat>,
    /// What this client is holding down right now.
    held: Held,
}

/// What a client has pressed and not yet let go of.
///
/// Tracked because a libei client is a process at the other end of a socket,
/// and processes are killed mid-drag. Nothing about the seat notices on its
/// own: a button pressed and never released is a button the focused client
/// believes is still down, which is a window that goes on being dragged by a
/// pointer nobody is moving. A modifier is worse — Shift left latched turns
/// every keystroke after it into the wrong character, and nothing corrects it
/// until the person at the machine happens to press and release that key
/// themselves.
#[derive(Default)]
struct Held {
    /// Keys from the ordinary keyboard device, as xkb keycodes. Let go of
    /// through `release_injected_key`, which undoes the bookkeeping their
    /// presses did on the way through `process_input_event`.
    keys: Vec<Keycode>,
    /// Keys pressed through the text device instead, as evdev codes. A list of
    /// its own because they were pressed by a different path — see
    /// [`ViewportState::eis_text_keysym`] — and a release has to travel the
    /// path its press did or it is not the same key going up.
    text_keys: Vec<u32>,
    /// Pointer buttons, by evdev code.
    buttons: Vec<u32>,
    /// How many fingers are down. A count rather than the slots, because there
    /// is only one thing to do about them and it is done to all of them at
    /// once: `wl_touch.cancel` ends the whole sequence, which is exactly what
    /// a touchscreen that has been unplugged mid-gesture means.
    touches: usize,
}

impl Held {
    /// Remember a press, or forget the release that answers it.
    ///
    /// One function for the three lists because the mistake to avoid is the
    /// same in each and is easy to make twice: a press remembered twice is a
    /// key released twice on disconnect, and a release that removes nothing
    /// leaves a key remembered after it is already up — which is the same key
    /// released again later, to a client that has long since moved on.
    fn note<T: PartialEq>(list: &mut Vec<T>, what: T, pressed: bool) {
        match list.iter().position(|held| *held == what) {
            Some(at) if !pressed => {
                list.remove(at);
            }
            None if pressed => list.push(what),
            // A press of something already held, or a release of something
            // that was not: the client is repeating itself or answering an
            // event this end never saw. Neither changes what is down.
            _ => {}
        }
    }
}

impl ViewportState {
    /// Hand a libei client the seat its session was granted.
    ///
    /// Everything here runs on the compositor thread: the context is built
    /// here, the source is inserted here, and the events it produces are
    /// routed here. The bus thread's part was making the socket and deciding
    /// that this session is allowed one.
    ///
    /// A second call for a session that already has a connection replaces it.
    /// Refusing the second instead would leave an application whose client
    /// library dropped its context — which is an ordinary thing for one to do
    /// — with a session that can never be driven again, and there is nothing
    /// to protect by keeping the first: it is the same grant, and its peer has
    /// stopped reading or the application would not be asking for another.
    pub fn connect_eis(&mut self, session: OwnedObjectPath, stream: UnixStream, devices: u32) {
        // Before the new one is built, so that at no point are two sockets for
        // one session both able to type.
        self.revoke_eis(&session);

        let context = match eis::Context::new(stream) {
            Ok(context) => context,
            // Nothing to tell the application: ConnectToEIS answered before
            // this ran, so what it has is an fd whose peer is about to be
            // dropped, which it reads as the server closing the connection.
            Err(e) => {
                tracing::warn!("remote desktop: could not start an EI context: {e}");
                return;
            }
        };

        // The session the events belong to, carried into the callback so that
        // every event can find the grant and the held state that go with it.
        // By value rather than by reference for the obvious reason — the
        // closure outlives this call — and cheap: it is a D-Bus path.
        let path = session.clone();
        let token = match self.loop_handle.insert_source(
            EiInput::new(context),
            move |event, connection, state| {
                state.eis_event(&path, event, connection);
            },
        ) {
            Ok(token) => token,
            Err(e) => {
                tracing::warn!("remote desktop: could not listen on an EI socket: {e}");
                return;
            }
        };

        tracing::info!(
            "remote desktop: {session} is connected over EI and may drive the {}",
            crate::screencast::remote::device_names(devices).join(", ")
        );
        self.eis.live.insert(
            session,
            Connection {
                token,
                devices,
                seat: None,
                held: Held::default(),
            },
        );
    }

    /// Take a live libei connection away from a session that is over.
    ///
    /// Called when the frontend closes the session, when the frontend itself
    /// goes away — the two ways a grant ends — and from
    /// [`ViewportState::connect_eis`] when a session asks for a second socket.
    /// Silent when there is nothing to revoke, which is the ordinary case: the
    /// great majority of remote-desktop sessions never call ConnectToEIS at
    /// all, and every one of them is closed.
    pub fn revoke_eis(&mut self, session: &OwnedObjectPath) {
        let Some(connection) = self.eis.live.remove(session) else {
            return;
        };
        tracing::info!("remote desktop: taking the EI socket away from {session}");
        // The source owns the context, which owns the socket, so removing it
        // is what the client actually notices. Done before the held keys are
        // let go of, so that nothing the client sent in the meantime can be
        // read out of the socket and press something after it was released.
        self.loop_handle.remove(connection.token);
        self.release_held(connection.held);
    }

    /// One event from a libei client.
    ///
    /// The dispatcher. `session` is which client it came from, and it is
    /// needed for every event rather than only for the connection ones because
    /// what a client is holding down is per client.
    fn eis_event(
        &mut self,
        session: &OwnedObjectPath,
        event: EiInputEvent,
        connection: &mut EiInputConnection,
    ) {
        match event {
            EiInputEvent::Connected => self.eis_connected(session, connection),
            EiInputEvent::Event(event) => self.eis_input(session, event),
            EiInputEvent::TextKeysym { keysym, state } => {
                self.eis_text_keysym(session, keysym, state == KeyState::Pressed)
            }
            EiInputEvent::TextUtf8 { text } => self.eis_text(&text),
            // The source takes itself out of the loop when it produces this,
            // so the token must not be removed again — a token is an index
            // into calloop's own table and one that has been freed may name
            // somebody else's source by the time it is used. Taking the row
            // out here is also what stops `revoke_eis` from doing exactly
            // that when the session is closed a moment later, which is the
            // usual order: a client disconnects and its application then tidies
            // up its portal session.
            EiInputEvent::Disconnected => {
                tracing::info!("remote desktop: the EI client for {session} disconnected");
                if let Some(connection) = self.eis.live.remove(session) {
                    self.release_held(connection.held);
                }
            }
        }
    }

    /// The client has finished its handshake, so it can be given devices.
    ///
    /// This is where consent is spent. Only the granted devices are added, so
    /// the client's own `ei_seat.bind` cannot reach anything else: the
    /// capabilities are all advertised on the seat, because libei has no way
    /// to advertise fewer, but binding one this end did not create produces no
    /// device and therefore no events. A client that asks for a keyboard on a
    /// pointer-only grant is not refused so much as handed nothing to type
    /// with.
    fn eis_connected(&mut self, session: &OwnedObjectPath, connection: &mut EiInputConnection) {
        let Some(devices) = self.eis.live.get(session).map(|live| live.devices) else {
            // The session was closed between the socket being handed out and
            // the handshake finishing. The source is already gone with it, so
            // there is nothing to add devices to.
            return;
        };
        let regions = self.eis_regions();
        let seat = connection.add_seat(SEAT_NAME);

        if devices & DEVICE_KEYBOARD != 0 {
            // The keymap the seat is actually using, not a default one. A
            // client is sent this keymap and composes keycodes against it, so
            // a Dvorak desk handed a US keymap types transposed letters and
            // nothing anywhere says why.
            let configured = self.keyboard_config.clone();
            let xkb = XkbConfig {
                layout: configured.layout.as_deref().unwrap_or(""),
                variant: configured.variant.as_deref().unwrap_or(""),
                options: configured.options.clone(),
                ..Default::default()
            };
            match seat.add_keyboard(KEYBOARD_NAME, xkb) {
                Ok(()) => {}
                // The same refusal `apply_config` handles when the layout was
                // set: the compositor keeps working and the client gets no
                // keyboard, which is better than a keyboard whose keymap does
                // not match the one the desk is typing on.
                Err(e) => tracing::warn!("remote desktop: no EI keyboard for {session}: {e}"),
            }
            // Typing by keysym or by string, which is the same permission: a
            // text device presses keys, and the grant that allows one allows
            // the other. It is worth having because a keycode can only produce
            // what the keymap has — an application sending a character the
            // desk's layout cannot type has no other way to send it.
            seat.add_text(TEXT_NAME);
        }

        if devices & DEVICE_POINTER != 0 {
            seat.add_pointer(POINTER_NAME);
            // Both kinds, because they answer different questions and an
            // application uses whichever its own end has: a remote-control
            // tool sends the position the operator clicked in the picture it
            // is showing, and something driving a game sends the movement.
            seat.add_pointer_absolute(ABSOLUTE_NAME, &regions);
        }

        if devices & DEVICE_TOUCHSCREEN != 0 {
            seat.add_touch(TOUCH_NAME, &regions);
        }

        if let Some(live) = self.eis.live.get_mut(session) {
            live.seat = Some(seat);
        }
    }

    /// An input event from a libei client, on its way to the seat.
    ///
    /// Most of it is handed straight to `process_input_event`, which is the
    /// point of using smithay's wrapper at all: `EiInput` is an `InputBackend`
    /// like libinput is, so a remote keystroke is filtered by the same shortcut
    /// table a real one is, wakes the screens the same way, and counts as the
    /// same activity against the idle timer. The Notify path cannot do that —
    /// its events are not `InputEvent`s and never were — which is why it has
    /// its own `inject_*` helpers and why those stay where they are.
    ///
    /// Two kinds of event are not handed over unchanged, and both are about
    /// coordinates. `process_input_event` reads an absolute position as a
    /// fraction of one output, because that is what a touchscreen and a tablet
    /// report; a libei client answers in the layout's own coordinates, because
    /// that is what `ei_device.region` told it to use. Scaling those against an
    /// output would put a remote pointer somewhere the operator did not click.
    /// So the position is taken as it stands and handed to the same code the
    /// other arm ends in — see [`ViewportState::pointer_absolute_to`].
    fn eis_input(&mut self, session: &OwnedObjectPath, event: InputEvent<EiInput>) {
        match event {
            InputEvent::Keyboard { event } => {
                let code = event.key_code();
                let pressed = event.state() == KeyState::Pressed;
                if let Some(held) = self.held_by(session) {
                    Held::note(&mut held.keys, code, pressed);
                }
                self.process_input_event::<EiInput>(InputEvent::Keyboard { event });
            }
            InputEvent::PointerButton { event } => {
                let button = event.button_code();
                let pressed = event.state() == smithay::backend::input::ButtonState::Pressed;
                if let Some(held) = self.held_by(session) {
                    Held::note(&mut held.buttons, button, pressed);
                }
                self.process_input_event::<EiInput>(InputEvent::PointerButton { event });
            }
            InputEvent::PointerMotionAbsolute { event } => {
                let at = (event.x(), event.y()).into();
                self.pointer_absolute_to(at, event.time());
            }
            InputEvent::TouchDown { event } => {
                if let Some(held) = self.held_by(session) {
                    held.touches += 1;
                }
                self.touch_down_at(event.slot(), (event.x(), event.y()).into(), event.time());
            }
            InputEvent::TouchMotion { event } => {
                self.touch_motion_at(event.slot(), (event.x(), event.y()).into(), event.time());
            }
            InputEvent::TouchUp { event } => {
                if let Some(held) = self.held_by(session) {
                    held.touches = held.touches.saturating_sub(1);
                }
                self.process_input_event::<EiInput>(InputEvent::TouchUp { event });
            }
            InputEvent::TouchCancel { event } => {
                if let Some(held) = self.held_by(session) {
                    held.touches = 0;
                }
                self.process_input_event::<EiInput>(InputEvent::TouchCancel { event });
            }
            // A device the client has just bound. Handed on like the rest —
            // it means nothing different coming from a socket — and then used
            // for the one thing only this moment can do: a keyboard that has
            // this instant appeared is a keyboard that can now be told what
            // the modifiers are. Before it, there was nothing to tell.
            InputEvent::DeviceAdded { device } => {
                self.process_input_event::<EiInput>(InputEvent::DeviceAdded { device });
                self.prime_eis_modifiers(session);
            }
            // Relative motion, scrolling, the frame that ends a set of touches,
            // and the devices going away as the client unbinds them. None of
            // them can leave anything held and none of them mean anything
            // different coming from a socket.
            event => self.process_input_event::<EiInput>(event),
        }
    }

    /// A key pressed by keysym rather than by keycode, through the text device.
    ///
    /// `inject_keysym` is the compositor's existing answer to "type this
    /// symbol" and this is deliberately the same journey — the keysym is
    /// looked up in the seat's keymap and the key that produces it is pressed —
    /// but done a step at a time here, because the code that was pressed is
    /// what has to be released if the client disappears while holding it, and
    /// `inject_keysym` does not say which one it used.
    fn eis_text_keysym(&mut self, session: &OwnedObjectPath, keysym: u32, pressed: bool) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        let keysym = Keysym::new(keysym);
        let Some(code) = keyboard.keycode_for_keysym(keysym) else {
            tracing::debug!(
                "remote desktop: no key in this layout produces {}",
                smithay::input::keyboard::xkb::keysym_get_name(keysym)
            );
            return;
        };
        // Back to evdev's numbering, which is what `inject_key` takes: xkb's
        // codes are offset by eight from it.
        let code = code.raw().saturating_sub(8);
        if let Some(held) = self.held_by(session) {
            Held::note(&mut held.text_keys, code, pressed);
        }
        self.inject_key(code, pressed);
    }

    /// A string the client asked to have typed.
    ///
    /// Pressed and released a character at a time, which is what the text
    /// device is for: an application that has a string and no idea what
    /// keyboard is at the other end. Nothing is remembered as held, because
    /// nothing is left down — a character that cannot be typed on this layout
    /// is skipped by `inject_keysym`, with its own line in the log.
    fn eis_text(&mut self, text: &str) {
        for character in text.chars() {
            let keysym = smithay::input::keyboard::xkb::utf32_to_keysym(character as u32);
            self.inject_keysym(keysym.raw(), true);
            self.inject_keysym(keysym.raw(), false);
        }
    }

    /// What a given client is holding down, if it is still connected.
    fn held_by(&mut self, session: &OwnedObjectPath) -> Option<&mut Held> {
        self.eis.live.get_mut(session).map(|live| &mut live.held)
    }

    /// Let go of everything a client that has gone was holding.
    ///
    /// In the order a person would: keys first, because a latched modifier
    /// changes what everything else means, then the buttons, then the fingers.
    fn release_held(&mut self, held: Held) {
        for code in held.keys {
            self.release_injected_key(code);
        }
        for code in held.text_keys {
            self.inject_key(code, false);
        }
        for button in held.buttons {
            self.inject_button(button, false);
        }
        // Cancelled rather than lifted: a finger whose device has gone did not
        // finish its gesture, and a client told it was lifted would act on the
        // tap or the swipe that the operator never completed.
        if held.touches > 0 {
            self.inject_touch_cancel();
        }
    }

    /// The coordinate spaces a libei client may point within.
    ///
    /// One per monitor, in the layout's own coordinates, which is what makes a
    /// multi-monitor desk reachable at all: a client sends a position inside
    /// one of these and this end already knows where that is. The scale is the
    /// monitor's, so a client that is looking at a 200% screen's real pixels
    /// can convert; the mapping id is the connector name, which is what ties a
    /// region to the screencast stream showing the same monitor — a
    /// remote-control tool watching DP-1 and clicking in it needs to know which
    /// region that picture is of.
    fn eis_regions(&self) -> Vec<EiRegion> {
        self.space
            .outputs()
            .filter_map(|output| {
                Some(EiRegion {
                    rect: self.space.output_geometry(output)?,
                    scale: output.current_scale().fractional_scale() as f32,
                    mapping_id: Some(output.name()),
                })
            })
            .collect()
    }

    /// Tell every connected client what the seat's modifiers are now.
    ///
    /// `ei_keyboard.modifiers` is how a client learns that the compositor's
    /// keyboard state changed under it. It has two sources and a client can
    /// see neither: the person at the machine pressing Shift, and a key the
    /// client itself sent latching Caps Lock. A client composes its keystrokes
    /// against its own idea of the state — press Shift, press a, release both,
    /// for a capital — so a state that drifted is a remote session typing
    /// capitals nobody asked for until something happens to resynchronise it.
    ///
    /// Sent only on a change, because ordinary typing at the desk moves the
    /// modifier state twice per shifted character and a remote client has
    /// nothing to do with most of that. Reading the state is a lock on the
    /// seat's keyboard, so the empty case comes first: a desktop with no
    /// remote session pays nothing for this at all.
    ///
    /// The order the four numbers go in is libei's rather than
    /// `wl_keyboard`'s; see [`modifier_args`].
    pub fn sync_eis_modifiers(&mut self) {
        if self.eis.live.is_empty() {
            return;
        }
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        let mods = keyboard.modifier_state().serialized;
        if !self.eis.changed(mods) {
            return;
        }
        let (depressed, locked, latched, group) = modifier_args(mods);
        for connection in self.eis.live.values() {
            if let Some(seat) = connection.seat.as_ref() {
                seat.keyboard_modifiers(depressed, locked, latched, group);
            }
        }
    }

    /// Tell one client the modifier state it has just become able to hear.
    ///
    /// For the client that has this moment bound a keyboard. Until it does,
    /// `ei_keyboard.modifiers` has no device to go to and saying it is the
    /// same as not saying it — so a session that started while Caps Lock was
    /// on would otherwise be told about it only when somebody pressed Caps
    /// Lock again. The broadcast above cannot serve this: it sends on a
    /// change, and a client arriving into an unchanged state needs the state
    /// rather than the change.
    fn prime_eis_modifiers(&mut self, session: &OwnedObjectPath) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        let mods = keyboard.modifier_state().serialized;
        let Some(seat) = self
            .eis
            .live
            .get(session)
            .and_then(|connection| connection.seat.as_ref())
        else {
            return;
        };
        let (depressed, locked, latched, group) = modifier_args(mods);
        seat.keyboard_modifiers(depressed, locked, latched, group);
        // So the next broadcast is measured against what everybody has now
        // been told, rather than re-sending on the first unrelated change.
        self.eis.changed(mods);
    }

    /// Tell every connected client where the monitors are now.
    ///
    /// Called when the layout changes, because a region is fixed when the
    /// device is made: a client told about a screen that has since been
    /// unplugged, moved or rescaled goes on pointing into a coordinate space
    /// that no longer describes the desk. Smithay answers that by replacing the
    /// absolute devices, so the client sees them go and come back — which is
    /// why this is not called on every commit but only where the layout is
    /// settled.
    pub fn refresh_eis_regions(&mut self) {
        if self.eis.live.is_empty() {
            return;
        }
        let regions = self.eis_regions();
        for connection in self.eis.live.values() {
            if let Some(seat) = connection.seat.as_ref() {
                seat.update_regions(&regions);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A press is remembered once and a release forgets it. Both halves
    /// matter: the first is a key released twice on disconnect, the second a
    /// key released to a client that let go of it a minute ago.
    #[test]
    fn what_is_held_is_what_was_pressed() {
        let mut held: Vec<u32> = Vec::new();
        Held::note(&mut held, 0x110, true);
        assert_eq!(held, [0x110]);
        // A repeat is not a second press. A libei client may send one — key
        // repeat is the client's own business — and two entries would be two
        // releases.
        Held::note(&mut held, 0x110, true);
        assert_eq!(held, [0x110]);
        Held::note(&mut held, 0x111, true);
        Held::note(&mut held, 0x110, false);
        assert_eq!(held, [0x111]);
        // And a release of something never pressed leaves nothing behind.
        Held::note(&mut held, 0x112, false);
        assert_eq!(held, [0x111]);
    }

    fn mods(depressed: u32, latched: u32, locked: u32) -> smithay::input::keyboard::SerializedMods {
        smithay::input::keyboard::SerializedMods {
            depressed,
            latched,
            locked,
            layout_effective: 0,
        }
    }

    /// libei takes locked before latched, and `wl_keyboard` takes them the
    /// other way round. Both are bitmasks over the same modifiers, so getting
    /// it wrong compiles and runs — a held Shift arrives at the client as a
    /// latched one, which is a remote session that keeps shifting long after
    /// the key came up.
    #[test]
    fn the_argument_order_is_libeis_and_not_waylands() {
        let (depressed, locked, latched, group) =
            modifier_args(smithay::input::keyboard::SerializedMods {
                depressed: 1,
                latched: 2,
                locked: 4,
                layout_effective: 8,
            });
        assert_eq!((depressed, locked, latched, group), (1, 4, 2, 8));
    }

    /// Only a change is worth a message. Ordinary typing at the desk moves the
    /// modifier state twice per shifted character, and a remote client has
    /// nothing to do with most of that — without this every keystroke somebody
    /// types puts two more messages on every remote socket.
    #[test]
    fn the_same_state_twice_is_said_once() {
        let mut connections = Connections::default();
        assert!(connections.changed(mods(0, 0, 0)), "nothing was ever sent");
        assert!(!connections.changed(mods(0, 0, 0)));
        assert!(connections.changed(mods(1, 0, 0)), "shift went down");
        assert!(!connections.changed(mods(1, 0, 0)));
        assert!(connections.changed(mods(0, 0, 0)), "and came back up");
    }

    /// A latched modifier and a locked one are not the same state, even where
    /// the same bits are set. Caps Lock latching and Caps Lock locking are the
    /// difference between one capital and every capital after it.
    #[test]
    fn latched_and_locked_are_different_news() {
        let mut connections = Connections::default();
        assert!(connections.changed(mods(0, 2, 0)));
        assert!(connections.changed(mods(0, 0, 2)));
    }
}
