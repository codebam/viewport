// SPDX-License-Identifier: GPL-3.0-or-later
//
// org.freedesktop.impl.portal.RemoteDesktop.
//
// The interface xdg-desktop-portal calls when an application asks to drive
// this machine — a remote-support tool, a screen-sharing client that lets the
// other end take the mouse, a test harness. It is the ScreenCast interface
// with input added: the same session handle, the same request objects, the
// same response codes, and — where the application also wants to see what it
// is typing into — the same PipeWire stream. That is why it lives beside
// `portal.rs` and shares its session table rather than standing up a second
// one; see `portal::Session`.
//
// The conversation is three calls again. CreateSession makes the handle,
// SelectDevices says which of keyboard, pointer and touchscreen are wanted,
// and Start asks the person at the machine and answers with the set that was
// actually granted. After that there are two ways for input to arrive, and
// both are implemented here.
//
// The first is the Notify calls: one D-Bus call per input event, forwarded by
// the frontend, checked here against the grant and handed to the compositor
// over a channel. The second is ConnectToEIS, which version two of the
// interface added and which answers with a libei socket the application
// speaks directly — see `crate::libei` for the server behind it and for what
// this end does and deliberately does not do on the bus thread. The Notify
// path is not deprecated by it and is not going anywhere: an application
// picks, and a frontend talking to an older implementation only has the one.
//
// Two decisions are worth stating outright, because both are refusals.
//
// A remote-desktop session is never restored from a token. ScreenCast has
// `restore_data` so that a recorder set up in March still records the right
// window in June without anybody at the keyboard, and that is the right trade
// for a picture. It is the wrong one for a keyboard: a stored grant is a
// process that can type into this machine on the strength of a blob in the
// permission store, and the whole point of the chooser is that a person said
// yes. So `persist_mode` is answered with zero and every session asks.
//
// And a session is refused outright when there is no desktop page to ask
// through. A screen share falls back to sharing the focused window in that
// case, with a line in the log, because a compositor running without its shell
// is a test or a crash and the alternative is a portal that never works. The
// same fallback here would be a machine that hands over its keyboard because
// its own user interface is broken, which is not a trade to make silently.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

use super::portal::{release_session, Message, SessionObject, Sessions, Started as CastStarted};

/// Where every portal object this compositor serves lives on the bus.
const OBJECT_PATH: &str = "/org/freedesktop/portal/desktop";

/// The devices the interface knows about, as it numbers them.
pub const DEVICE_KEYBOARD: u32 = 1;
pub const DEVICE_POINTER: u32 = 2;
pub const DEVICE_TOUCHSCREEN: u32 = 4;

/// Everything this compositor can be driven with.
///
/// All three, because the compositor's seat has all three: `inject_key`,
/// `inject_pointer` and `inject_touch_down` in `input.rs` are the same paths
/// the control socket and — later — the on-screen keyboard use, and a device
/// this refused to offer would be one those paths already support.
pub const ALL_DEVICES: u32 = DEVICE_KEYBOARD | DEVICE_POINTER | DEVICE_TOUCHSCREEN;

/// The response codes, which are the portal's own and shared with ScreenCast.
const RESPONSE_SUCCESS: u32 = 0;
const RESPONSE_CANCELLED: u32 = 1;
const RESPONSE_FAILED: u32 = 2;

/// How a key or a button is said to be down.
///
/// The interface spells the state as a number rather than a boolean, and it is
/// the same number for both. Zero is up.
const STATE_PRESSED: u32 = 1;

/// Which way a discrete scroll went, as NotifyPointerAxisDiscrete numbers it.
const AXIS_VERTICAL: u32 = 0;
const AXIS_HORIZONTAL: u32 = 1;

/// How far one notch of a wheel scrolls, in the units `wl_pointer.axis`
/// carries.
///
/// NotifyPointerAxisDiscrete counts notches and Wayland clients read a
/// distance, so something has to convert. Fifteen is what libinput reports for
/// one detent of an ordinary mouse wheel and therefore what every toolkit is
/// tuned against — a smaller number scrolls a remote session more slowly than
/// the same wheel would locally, which reads as lag rather than as a setting.
const NOTCH: f64 = 15.0;

/// One input event on its way to the seat.
///
/// Flat rather than carrying the session it came from: whether the session was
/// allowed to send it is decided on the bus thread, against the grant in
/// `portal::Session`, before the message is built at all. What reaches the
/// compositor is a movement, and the only question left there is where on the
/// desk it lands.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Injection {
    /// A mouse that moved by this much, in the layout's own units.
    PointerMotion {
        dx: f64,
        dy: f64,
    },
    /// A pointer put at a place inside one of the streams this session was
    /// given, which is the only coordinate space a remote application has:
    /// it is looking at a picture of a monitor or of a window and clicking on
    /// what it sees, and where that is on the desk is this end's to work out.
    PointerMotionAbsolute {
        stream: u32,
        x: f64,
        y: f64,
    },
    PointerButton {
        button: u32,
        pressed: bool,
    },
    /// Smooth scrolling, in the same units `wl_pointer.axis` carries.
    PointerAxis {
        dx: f64,
        dy: f64,
        finish: bool,
    },
    /// A wheel, in notches. Kept apart from the smooth kind all the way down
    /// so the v120 value a modern client reads is a whole number of detents
    /// rather than a distance divided back into one.
    PointerAxisDiscrete {
        axis: u32,
        steps: i32,
    },
    /// A key by its evdev code, which is what a keyboard sends.
    KeyboardKeycode {
        keycode: i32,
        pressed: bool,
    },
    /// A key by what it should type, which is what an application that has
    /// only a character to send has.
    KeyboardKeysym {
        keysym: i32,
        pressed: bool,
    },
    TouchDown {
        stream: u32,
        slot: u32,
        x: f64,
        y: f64,
    },
    TouchMotion {
        stream: u32,
        slot: u32,
        x: f64,
        y: f64,
    },
    TouchUp {
        slot: u32,
    },
}

impl Injection {
    /// Which device this event would be coming from, so a session that was
    /// granted one and not another cannot send it anyway.
    ///
    /// The check that matters, and the reason it is a method here rather than
    /// a line in each of the eleven Notify methods: a grant of the pointer
    /// alone must not become a grant of the keyboard because one of them
    /// forgot to ask.
    pub fn device(&self) -> u32 {
        match self {
            Self::PointerMotion { .. }
            | Self::PointerMotionAbsolute { .. }
            | Self::PointerButton { .. }
            | Self::PointerAxis { .. }
            | Self::PointerAxisDiscrete { .. } => DEVICE_POINTER,
            Self::KeyboardKeycode { .. } | Self::KeyboardKeysym { .. } => DEVICE_KEYBOARD,
            Self::TouchDown { .. } | Self::TouchMotion { .. } | Self::TouchUp { .. } => {
                DEVICE_TOUCHSCREEN
            }
        }
    }

    /// Whether every coordinate this event carries is a real number.
    ///
    /// The bus is free to carry NaN and infinities, and the seat is not: an
    /// absolute position is stored as the pointer's location, and every later
    /// motion is computed from it, so one NaN would stick and leave the
    /// pointer dead for the rest of the session — including after the remote
    /// grant is revoked. Non-finite events are dropped at this boundary.
    pub fn is_finite(&self) -> bool {
        match self {
            Self::PointerMotion { dx, dy } | Self::PointerAxis { dx, dy, .. } => {
                dx.is_finite() && dy.is_finite()
            }
            Self::PointerMotionAbsolute { x, y, .. }
            | Self::TouchDown { x, y, .. }
            | Self::TouchMotion { x, y, .. } => x.is_finite() && y.is_finite(),
            Self::PointerButton { .. }
            | Self::PointerAxisDiscrete { .. }
            | Self::KeyboardKeycode { .. }
            | Self::KeyboardKeysym { .. }
            | Self::TouchUp { .. } => true,
        }
    }
}

/// What a remote-desktop session was given.
#[derive(Debug, Clone)]
pub struct Started {
    /// The devices the user allowed, which is never more than was asked for
    /// and may be less.
    pub devices: u32,
    /// The stream the application also asked to watch, if it asked. Exactly
    /// what ScreenCast.Start would have answered with, because it is the same
    /// share started the same way — the difference is only that the consent
    /// covered typing into it as well.
    pub cast: Option<CastStarted>,
}

/// The names to show the person being asked, in a fixed order.
///
/// Fixed rather than in the order the bits happen to be set, because the
/// sentence is read rather than parsed and "keyboard and mouse" is the phrase
/// somebody is expecting. Bits this end does not know about are dropped: the
/// grant is masked to [`ALL_DEVICES`] before anything is asked, so naming one
/// would be describing a permission that is not being given.
pub fn device_names(devices: u32) -> Vec<String> {
    [
        (DEVICE_KEYBOARD, "keyboard"),
        (DEVICE_POINTER, "mouse"),
        (DEVICE_TOUCHSCREEN, "touchscreen"),
    ]
    .into_iter()
    .filter(|(bit, _)| devices & bit != 0)
    .map(|(_, name)| name.to_owned())
    .collect()
}

/// The object on the bus.
///
/// Holds the same [`Sessions`] the ScreenCast object holds, which is what lets
/// the two interfaces be halves of one conversation, and the same channel to
/// the compositor: a remote-desktop session that also shares the screen starts
/// its stream through exactly the code a plain share does.
pub struct RemoteDesktop {
    sender: smithay::reexports::calloop::channel::Sender<Message>,
    sessions: Sessions,
}

impl RemoteDesktop {
    pub fn new(
        sender: smithay::reexports::calloop::channel::Sender<Message>,
        sessions: Sessions,
    ) -> Self {
        Self { sender, sessions }
    }

    /// Whether a call came from the portal frontend.
    ///
    /// The shared check, for the reason spelled out where it lives: this
    /// interface hands out the keyboard, and the bus it is on is one every
    /// process in the session can reach.
    fn called_by_frontend(&self, header: &zbus::message::Header<'_>) -> bool {
        super::portal::called_by_frontend(&self.sessions, "remote desktop", header)
    }

    /// Send one input event, if this session was allowed to.
    ///
    /// Everything the eleven Notify methods have in common, which is nearly
    /// all of them: find the session, check the grant covers the device the
    /// event would come from, hand it to the compositor. Written once because
    /// eleven copies of a permission check is eleven chances to leave one out,
    /// and the one left out would be a session typing into a machine that
    /// agreed to a mouse.
    ///
    /// A refusal is a log line and nothing else. The Notify methods have no
    /// return value — the interface defines them as one-way — so there is
    /// nowhere to say no to the application, and dropping the event is the
    /// only refusal available.
    fn notify(
        &self,
        session_handle: ObjectPath<'_>,
        header: &zbus::message::Header<'_>,
        injection: Injection,
    ) {
        if !self.called_by_frontend(header) {
            return;
        }
        let path = OwnedObjectPath::from(session_handle);
        let granted = self
            .sessions
            .lock()
            .unwrap()
            .sessions
            .get(&path)
            .map(|session| session.granted_devices)
            .unwrap_or(0);
        let wanted = injection.device();
        if granted & wanted == 0 {
            // Rate-limited by being at debug: a remote pointer sends hundreds
            // of these a second, and a session that was refused one is a
            // session that will be refused all of them. The line that matters
            // — the grant itself — is at info, in the compositor.
            tracing::debug!(
                "remote desktop: dropping {injection:?} for {path}, which was granted {granted}"
            );
            return;
        }
        if !injection.is_finite() {
            tracing::debug!(
                "remote desktop: dropping {injection:?} for {path}, which is not a number"
            );
            return;
        }
        if self.sender.send(Message::Inject(injection)).is_err() {
            tracing::warn!("remote desktop: the compositor is not listening");
        }
    }

    /// Undo the mark that says a session is holding a libei socket.
    ///
    /// For the two ways handing one out can fail after the session has been
    /// marked. A session left marked would send a revocation on close for a
    /// connection that was never made, which the compositor ignores — so this
    /// is tidiness rather than safety, and it is worth having because the mark
    /// is also what a reader of the table would take as "this session has a
    /// socket", and it would be wrong.
    fn forget_eis(&self, path: &OwnedObjectPath) {
        if let Some(session) = self.sessions.lock().unwrap().sessions.get_mut(path) {
            session.eis = false;
        }
    }

    /// Ask the compositor, and wait for it.
    ///
    /// The same shape as the ScreenCast side's `ask`, and awaited for the same
    /// reason: the answer is a person deciding, which takes as long as it
    /// takes, and this call is running on the bus connection the rest of the
    /// desktop's settings traffic shares.
    async fn ask(
        &self,
        message: Message,
        reply: async_channel::Receiver<Result<Started, String>>,
    ) -> Result<Started, String> {
        self.sender
            .send(message)
            .map_err(|_| "the compositor is not listening".to_owned())?;
        reply
            .recv()
            .await
            .map_err(|_| "the compositor did not answer".to_owned())?
    }
}

#[zbus::interface(name = "org.freedesktop.impl.portal.RemoteDesktop")]
impl RemoteDesktop {
    /// Everything the seat has.
    #[zbus(property, name = "AvailableDeviceTypes")]
    fn available_device_types(&self) -> u32 {
        ALL_DEVICES
    }

    /// Version two of the interface.
    ///
    /// Two is the version that adds ConnectToEIS, and there is a real EI
    /// server behind it now — see [`RemoteDesktop::connect_to_eis`] and
    /// `crate::libei`. Claiming it is what lets the frontend offer an
    /// application the socket instead of the Notify calls, which is the
    /// difference between one D-Bus round trip per pointer movement and none.
    ///
    /// The rest of what version two describes is optional and answered
    /// deliberately. `restore_data` and `persist_mode` on SelectDevices are a
    /// remote-desktop grant restored from a token, which this end refuses on
    /// principle — see the note at the top of this file — so the option is
    /// read as the zero it defaults to and Start answers with no restore data.
    /// `clipboard_enabled` is answered explicitly, and it is false.
    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        2
    }

    /// The application is starting a conversation.
    ///
    /// The session goes into the table both interfaces read, marked as this
    /// one's, so that a SelectSources arriving on ScreenCast for the same
    /// handle is understood as this application also wanting to see the desk.
    async fn create_session(
        &self,
        _handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        app_id: &str,
        _options: HashMap<String, OwnedValue>,
        #[zbus(object_server)] server: &zbus::ObjectServer,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        if !self.called_by_frontend(&header) {
            return (RESPONSE_FAILED, HashMap::new());
        }
        let path = OwnedObjectPath::from(session_handle);
        let owner = header.sender().map(|name| name.to_string());
        tracing::debug!("remote desktop: create session {path} for {app_id:?}");
        // A handle coming back is a conversation being replaced, not resumed,
        // and a displaced remote-desktop row is the worst kind to drop
        // silently: its grant dies with the row, but its stream — if it had
        // one — keeps being composited, and its EI socket keeps carrying
        // input. Both are stopped here. See [`release_session`].
        //
        // Out of the lock before anything is awaited, for the same reason as
        // on the screen-share side.
        let displaced = self.sessions.lock().unwrap().sessions.insert(
            path.clone(),
            super::portal::Session::new(app_id, owner, true),
        );
        if let Some(displaced) = displaced {
            release_session(&displaced, &path, &self.sender);
            // And the old object off the bus, so the new one can take the
            // path: publishing over an interface that is already there is
            // quietly refused, and a Close from the frontend would then be
            // answered by the previous conversation's object.
            if let Err(e) = server.remove::<SessionObject, _>(&path).await {
                tracing::warn!("could not take a replaced session off the bus: {e}");
            }
        }

        // The same session object the screen-share path publishes, because it
        // has the same job: the frontend closes it when the application is
        // done, and closing it takes the row out of the table — which is what
        // revokes the grant. Without it a remote session would go on being
        // allowed to type for as long as the compositor was up.
        let session = SessionObject {
            path: path.clone(),
            sender: self.sender.clone(),
            sessions: self.sessions.clone(),
        };
        if let Err(e) = server.at(&path, session).await {
            tracing::warn!("could not publish a remote desktop session: {e}");
        }
        (RESPONSE_SUCCESS, HashMap::new())
    }

    /// Which devices the application wants to drive.
    ///
    /// Remembered rather than acted on, exactly as SelectSources is: nothing
    /// is granted until Start, which is where the person is asked. Masked to
    /// what this compositor has, so a bit from a later version of the
    /// interface cannot be granted by arithmetic.
    fn select_devices(
        &self,
        _handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        _app_id: &str,
        options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        if !self.called_by_frontend(&header) {
            return (RESPONSE_FAILED, HashMap::new());
        }
        let wanted = options
            .get("types")
            .and_then(|value| u32::try_from(value).ok())
            // An application that says nothing wants everything, which is what
            // the interface documents the default to be. It costs nothing to
            // be generous here: the user still sees the full list and still
            // has to agree to it.
            .unwrap_or(ALL_DEVICES)
            & ALL_DEVICES;

        let path = OwnedObjectPath::from(session_handle);
        let mut shared = self.sessions.lock().unwrap();
        let Some(session) = shared.sessions.get_mut(&path) else {
            tracing::warn!("remote desktop: select devices for a session that does not exist");
            return (RESPONSE_FAILED, HashMap::new());
        };
        tracing::debug!("remote desktop: select devices {wanted} for {path}");
        session.wanted_devices = wanted;
        (RESPONSE_SUCCESS, HashMap::new())
    }

    /// Ask the person at the machine, and hand back what they allowed.
    async fn start(
        &self,
        _handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        _app_id: &str,
        _parent_window: &str,
        _options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        if !self.called_by_frontend(&header) {
            return (RESPONSE_FAILED, HashMap::new());
        }
        let path = OwnedObjectPath::from(session_handle);
        let (devices, types, clipboard) = {
            let shared = self.sessions.lock().unwrap();
            match shared.sessions.get(&path) {
                // One Start per session, as on the screen-share side: a
                // second would create another stream and overwrite the node
                // the first is stopped by, so the grant would outlive the
                // Close that revokes it.
                Some(session) if !session.may_start() => {
                    tracing::warn!("remote desktop: refusing to start {path} a second time");
                    return (RESPONSE_FAILED, HashMap::new());
                }
                Some(session) => (
                    session.wanted_devices,
                    session.sources_selected.then_some(session.types),
                    session.clipboard,
                ),
                None => return (RESPONSE_FAILED, HashMap::new()),
            }
        };
        // An application that never called SelectDevices, or called it with
        // nothing, is asking for a session that can do nothing. Refused rather
        // than granted empty: putting a chooser up that says "this application
        // wants to control your computer with nothing" is a question with no
        // answer, and a session that is granted the empty set would sit there
        // sending events that are all dropped.
        if devices == 0 {
            tracing::warn!("remote desktop: refusing {path}, which asked for no devices");
            return (RESPONSE_CANCELLED, HashMap::new());
        }

        let (sender, receiver) = async_channel::bounded(1);
        let started = match self
            .ask(
                Message::StartRemote {
                    devices,
                    types,
                    reply: sender,
                },
                receiver,
            )
            .await
        {
            Ok(started) => started,
            Err(e) => {
                tracing::warn!("remote desktop: {e}");
                // Cancelled rather than failed, as on the screen-share side
                // and for the same reason: the ordinary case is a person
                // saying no, and an application shows a failure as an error
                // box rather than as a decision.
                return (RESPONSE_CANCELLED, HashMap::new());
            }
        };

        // Written down before the answer goes out, because the answer is what
        // lets the application start sending events and the check those events
        // are made against is this row.
        let recorded = {
            let mut shared = self.sessions.lock().unwrap();
            match shared.sessions.get_mut(&path) {
                Some(session) => session.record_remote_start(
                    started.devices,
                    started.cast.as_ref().map(|cast| cast.node),
                    clipboard,
                ),
                None => false,
            }
        };
        if !recorded {
            // The frontend went away while the chooser was up, and the watcher
            // has already taken the row out — or a raced second Start got
            // here first. Nothing to grant to, and, unlike the screen-share
            // side, something to stop: the chooser may have been granted a
            // stream along with the devices, started after the watcher looked,
            // and nobody else knows it exists. Left alone it is a compositor
            // compositing into a stream whose session is gone, forever.
            tracing::warn!(
                "remote desktop: {path} was closed or already started while it was being chosen"
            );
            if let Some(cast) = started.cast {
                let _ = self.sender.send(Message::Close { node: cast.node });
            }
            return (RESPONSE_CANCELLED, HashMap::new());
        }
        tracing::info!(
            "remote desktop: {path} may drive the {}",
            device_names(started.devices).join(", ")
        );

        let mut results: HashMap<String, OwnedValue> = HashMap::new();
        results.insert("devices".to_owned(), OwnedValue::from(started.devices));
        // Said explicitly either way. The frontend reads an absent key as
        // false, but an application reading the results dictionary sees the
        // difference between "no" and "this compositor did not think about it",
        // and a remote session pasting into the machine is the kind of thing
        // that should be a stated answer. True only when the session asked
        // through `RequestClipboard` before Start.
        results.insert("clipboard_enabled".to_owned(), OwnedValue::from(clipboard));

        if let Some(cast) = started.cast {
            // Described exactly as ScreenCast.Start describes it, because it
            // is the same stream: the consumer on the other end builds its
            // pw_stream from this and cannot tell which interface answered.
            let mut properties: HashMap<String, Value<'static>> = HashMap::new();
            properties.insert("size".to_owned(), Value::from((cast.width, cast.height)));
            properties.insert("source_type".to_owned(), Value::from(cast.source_type));
            let streams = vec![(cast.node, properties)];
            match OwnedValue::try_from(Value::from(streams)) {
                Ok(value) => {
                    results.insert("streams".to_owned(), value);
                    tracing::debug!(
                        "remote desktop: answering with node {} at {}x{}",
                        cast.node,
                        cast.width,
                        cast.height
                    );
                }
                Err(e) => tracing::warn!("remote desktop: could not describe the stream: {e}"),
            }
        }
        (RESPONSE_SUCCESS, results)
    }

    /// A mouse that moved, relative to wherever the cursor is now.
    ///
    /// There is deliberately no OpenPipeWireRemote on this interface: the
    /// specification puts it on ScreenCast, the frontend calls it there with
    /// this same session handle, and `portal.rs` answers it without caring
    /// which interface created the session. A remote-desktop session that also
    /// asked to see the desk therefore reaches PipeWire through exactly the
    /// code a plain screen share does.
    #[zbus(name = "NotifyPointerMotion")]
    fn notify_pointer_motion(
        &self,
        session_handle: ObjectPath<'_>,
        _options: HashMap<String, OwnedValue>,
        dx: f64,
        dy: f64,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) {
        self.notify(session_handle, &header, Injection::PointerMotion { dx, dy });
    }

    #[zbus(name = "NotifyPointerMotionAbsolute")]
    fn notify_pointer_motion_absolute(
        &self,
        session_handle: ObjectPath<'_>,
        _options: HashMap<String, OwnedValue>,
        stream: u32,
        x: f64,
        y: f64,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) {
        self.notify(
            session_handle,
            &header,
            Injection::PointerMotionAbsolute { stream, x, y },
        );
    }

    /// A button, by the evdev code the interface carries.
    ///
    /// Signed on the wire and unsigned everywhere below, because
    /// `wl_pointer.button` is unsigned and `BTN_LEFT` is 0x110 — the sign is
    /// an artefact of the interface's type, not a range anybody uses. A
    /// negative one is dropped rather than wrapped around into a button in the
    /// billions, which is the number a client would otherwise be handed.
    #[zbus(name = "NotifyPointerButton")]
    fn notify_pointer_button(
        &self,
        session_handle: ObjectPath<'_>,
        _options: HashMap<String, OwnedValue>,
        button: i32,
        state: u32,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) {
        let Ok(button) = u32::try_from(button) else {
            tracing::debug!("remote desktop: ignoring a negative button {button}");
            return;
        };
        self.notify(
            session_handle,
            &header,
            Injection::PointerButton {
                button,
                pressed: state == STATE_PRESSED,
            },
        );
    }

    /// Smooth scrolling. `finish` says the fingers left the touchpad, which is
    /// what stops a client's kinetic scroll.
    #[zbus(name = "NotifyPointerAxis")]
    fn notify_pointer_axis(
        &self,
        session_handle: ObjectPath<'_>,
        options: HashMap<String, OwnedValue>,
        dx: f64,
        dy: f64,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) {
        let finish = options
            .get("finish")
            .and_then(|value| bool::try_from(value).ok())
            .unwrap_or(false);
        self.notify(
            session_handle,
            &header,
            Injection::PointerAxis { dx, dy, finish },
        );
    }

    #[zbus(name = "NotifyPointerAxisDiscrete")]
    fn notify_pointer_axis_discrete(
        &self,
        session_handle: ObjectPath<'_>,
        _options: HashMap<String, OwnedValue>,
        axis: u32,
        steps: i32,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) {
        self.notify(
            session_handle,
            &header,
            Injection::PointerAxisDiscrete { axis, steps },
        );
    }

    #[zbus(name = "NotifyKeyboardKeycode")]
    fn notify_keyboard_keycode(
        &self,
        session_handle: ObjectPath<'_>,
        _options: HashMap<String, OwnedValue>,
        keycode: i32,
        state: u32,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) {
        self.notify(
            session_handle,
            &header,
            Injection::KeyboardKeycode {
                keycode,
                pressed: state == STATE_PRESSED,
            },
        );
    }

    #[zbus(name = "NotifyKeyboardKeysym")]
    fn notify_keyboard_keysym(
        &self,
        session_handle: ObjectPath<'_>,
        _options: HashMap<String, OwnedValue>,
        keysym: i32,
        state: u32,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) {
        self.notify(
            session_handle,
            &header,
            Injection::KeyboardKeysym {
                keysym,
                pressed: state == STATE_PRESSED,
            },
        );
    }

    // Eight parameters because the interface has eight: the session, the
    // options dictionary, the stream, the slot and two coordinates, plus the
    // receiver and the header zbus threads through. There is nothing to group
    // into a struct that would not have to be taken apart again to match the
    // signature the frontend calls.
    #[allow(clippy::too_many_arguments)]
    #[zbus(name = "NotifyTouchDown")]
    fn notify_touch_down(
        &self,
        session_handle: ObjectPath<'_>,
        _options: HashMap<String, OwnedValue>,
        stream: u32,
        slot: u32,
        x: f64,
        y: f64,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) {
        self.notify(
            session_handle,
            &header,
            Injection::TouchDown { stream, slot, x, y },
        );
    }

    // Eight parameters because the interface has eight: the session, the
    // options dictionary, the stream, the slot and two coordinates, plus the
    // receiver and the header zbus threads through. There is nothing to group
    // into a struct that would not have to be taken apart again to match the
    // signature the frontend calls.
    #[allow(clippy::too_many_arguments)]
    #[zbus(name = "NotifyTouchMotion")]
    fn notify_touch_motion(
        &self,
        session_handle: ObjectPath<'_>,
        _options: HashMap<String, OwnedValue>,
        stream: u32,
        slot: u32,
        x: f64,
        y: f64,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) {
        self.notify(
            session_handle,
            &header,
            Injection::TouchMotion { stream, slot, x, y },
        );
    }

    #[zbus(name = "NotifyTouchUp")]
    fn notify_touch_up(
        &self,
        session_handle: ObjectPath<'_>,
        _options: HashMap<String, OwnedValue>,
        slot: u32,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) {
        self.notify(session_handle, &header, Injection::TouchUp { slot });
    }

    /// A libei socket for a session that has already been granted one.
    ///
    /// The whole of what happens on this thread is: check the caller, check
    /// the grant, make a socket pair, send one half to the compositor and
    /// answer with the other. It deliberately builds no EI context and touches
    /// no compositor state — see `crate::libei` for why that division is
    /// forced rather than chosen, and for what the compositor does with the
    /// half it receives.
    ///
    /// Granted, not merely selected. An application that has called
    /// CreateSession and SelectDevices but not Start has asked for nothing yet
    /// and been given nothing, and a socket handed out at that point would be
    /// a way to drive the machine that never put a question on screen. So the
    /// test is `granted_devices`, which is zero until a person said yes, and
    /// the refusal for a session that has not got there is an error the
    /// application can read.
    ///
    /// The devices the socket may carry are the granted ones, sent along with
    /// it. Consent on this path cannot be a check per event — there are no
    /// events on this thread to check — so it is spent when the client's
    /// devices are created, which happens once and out of the client's reach.
    /// Named by hand because zbus would spell it `ConnectToEis`, from the
    /// method's own name, and the interface spells it `ConnectToEIS`. A method
    /// under the wrong name is not on the interface at all: the frontend calls
    /// the name in the specification and gets UnknownMethod back, which reads
    /// like a compositor that never implemented it.
    #[zbus(name = "ConnectToEIS")]
    fn connect_to_eis(
        &self,
        session_handle: ObjectPath<'_>,
        app_id: &str,
        _options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<zvariant::OwnedFd> {
        if !self.called_by_frontend(&header) {
            return Err(zbus::fdo::Error::AccessDenied(
                "that is not the portal frontend".to_owned(),
            ));
        }
        let path = OwnedObjectPath::from(session_handle);

        // The grant, read once. Marked as having a socket in the same lock:
        // the mark is what makes closing the session close the socket — see
        // [`Message::RevokeEis`] — and marking it after the fd had gone out
        // would leave a window in which a session could be closed while
        // holding a connection nobody would then revoke.
        let devices = {
            let mut shared = self.sessions.lock().unwrap();
            let Some(session) = shared.sessions.get_mut(&path) else {
                return Err(zbus::fdo::Error::InvalidArgs(format!(
                    "{path} is not a session this compositor knows"
                )));
            };
            if session.granted_devices == 0 {
                tracing::warn!(
                    "remote desktop: refusing an EI socket for {path}, which was granted nothing"
                );
                return Err(zbus::fdo::Error::AccessDenied(
                    "this session has not been granted any devices".to_owned(),
                ));
            }
            session.eis = true;
            session.granted_devices
        };

        // Two connected ends of one socket: `theirs` is the fd this call
        // answers with and `ours` is the one the compositor reads the EI
        // protocol out of. Named that way round because getting them the wrong
        // way round is a portal that hands an application the server's own end
        // and then waits for a handshake on the half nobody is speaking to.
        let (theirs, ours) = match std::os::unix::net::UnixStream::pair() {
            Ok(pair) => pair,
            Err(e) => {
                self.forget_eis(&path);
                return Err(zbus::fdo::Error::Failed(format!(
                    "could not make an EI socket: {e}"
                )));
            }
        };
        tracing::info!(
            "remote desktop: handing {app_id:?} an EI socket for {path}, with the {}",
            device_names(devices).join(", ")
        );
        if self
            .sender
            .send(Message::ConnectEis {
                session: path.clone(),
                stream: ours,
                devices,
            })
            .is_err()
        {
            self.forget_eis(&path);
            return Err(zbus::fdo::Error::Failed(
                "the compositor is not listening".to_owned(),
            ));
        }
        Ok(zvariant::OwnedFd::from(std::os::fd::OwnedFd::from(theirs)))
    }
}

/// How many remote clipboard reads may be blocked at once.
///
/// The same ceiling the local clipboard uses (`clipboard::MAX_THREADS`): a
/// peer that never writes the bytes it was asked for must not pin a thread and
/// a pipe per `SelectionWrite`, which would exhaust the process one D-Bus call
/// at a time.
const MAX_CLIPBOARD_READERS: usize = 8;

/// How long a silent selection writer is given before its reader reaps it.
const CLIPBOARD_IDLE: std::time::Duration = std::time::Duration::from_secs(5);

/// The longest one transfer may hold a reader, activity or not.
const CLIPBOARD_TOTAL: std::time::Duration = std::time::Duration::from_secs(30);

/// How long an unanswered `SelectionTransfer` may wait for `SelectionWrite`.
///
/// The reader has its own idle and total caps once a write starts; this one
/// bounds the time before that, so a serial cannot be answered long after the
/// `SetSelection` it belonged to was replaced. The same ceiling keeps the two
/// halves of one transfer from disagreeing about when it is too old.
const CLIPBOARD_TRANSFER: std::time::Duration = CLIPBOARD_TOTAL;

/// What a `SelectionWrite` serial names when it arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransferAnswer {
    /// The transfer is still outstanding and belongs to the asking session.
    Ready,
    /// It belonged to the asking session, but has waited too long to answer.
    Expired,
    /// No transfer was sent under this serial, or it belongs to another
    /// session.
    Unknown,
}

/// One `SelectionTransfer` this end has asked the frontend for.
#[derive(Debug)]
struct Transfer {
    /// The session the request was sent to, so a serial minted for one
    /// session cannot be answered on another.
    session: OwnedObjectPath,
    /// When the signal went out, so an old offer cannot be answered forever.
    sent: std::time::Instant,
}

/// The `SelectionTransfer`s this end has sent, and which of them still owns a
/// session's selection.
///
/// The serial is the frontend's token for one transfer: `SelectionWrite` may
/// only answer the transfer that serial names, from the session that was
/// asked, and only while the session still owns the selection. Without this
/// the serial was decorative and any granted session could write into the
/// local selection at any time, including after a later local copy. The
/// `owners` map is what turns a completed write into "the local side has the
/// selection now"; a newer `SetSelection` replaces it, so the old reader's
/// late answer cannot record over the new selection.
#[derive(Debug, Default)]
struct Transfers {
    /// Sent and not yet answered, by serial.
    outstanding: HashMap<u32, Transfer>,
    /// Answered with a pipe, by serial, until the reader thread finishes.
    writing: HashMap<u32, OwnedObjectPath>,
    /// The serial whose completion would make this session's selection local,
    /// one per session.
    owners: HashMap<OwnedObjectPath, u32>,
}

impl Transfers {
    /// Record a transfer just sent for `session`.
    fn requested(&mut self, session: &OwnedObjectPath, serial: u32, now: std::time::Instant) {
        if let Some(previous) = self.owners.insert(session.clone(), serial) {
            // A new `SetSelection` supersedes the old one. Its reader may
            // still be draining a pipe; `completed` sees the owner map moved
            // on and refuses to record what it read.
            self.outstanding.remove(&previous);
        }
        self.outstanding.insert(
            serial,
            Transfer {
                session: session.clone(),
                sent: now,
            },
        );
    }

    /// Answer a `SelectionWrite` for `serial`, if this session may write it.
    fn answered(
        &mut self,
        session: &OwnedObjectPath,
        serial: u32,
        now: std::time::Instant,
        ttl: std::time::Duration,
    ) -> TransferAnswer {
        let Some(transfer) = self.outstanding.remove(&serial) else {
            return TransferAnswer::Unknown;
        };
        if transfer.session != *session {
            self.outstanding.insert(serial, transfer);
            return TransferAnswer::Unknown;
        }
        if now.saturating_duration_since(transfer.sent) > ttl {
            if self.owners.get(session) == Some(&serial) {
                self.owners.remove(session);
            }
            return TransferAnswer::Expired;
        }
        self.writing.insert(serial, session.clone());
        TransferAnswer::Ready
    }

    /// Whether `SelectionWriteDone` names a write this session is in the
    /// middle of.
    fn written_by(&self, session: &OwnedObjectPath, serial: u32) -> bool {
        self.writing.get(&serial) == Some(session)
    }

    /// The reader thread has finished or given up. True when this transfer is
    /// still the one whose completion hands the selection to the local side;
    /// false when a newer `SetSelection`, or a release, has replaced it, in
    /// which case what was read must not overwrite the newer selection.
    fn completed(&mut self, session: &OwnedObjectPath, serial: u32) -> bool {
        self.writing.remove(&serial);
        if self.owners.get(session) == Some(&serial) {
            self.owners.remove(session);
            true
        } else {
            false
        }
    }

    /// The session gave the selection up (`SetSelection` with no mime types).
    /// True when it was still an owner, so a `SelectionOwnerChanged` false is
    /// owed.
    fn released(&mut self, session: &OwnedObjectPath) -> bool {
        self.outstanding
            .retain(|_, transfer| transfer.session != *session);
        self.writing.retain(|_, writer| writer != session);
        self.owners.remove(session).is_some()
    }
}

/// How many remote clipboard readers are alive.
static LIVE_CLIPBOARD_READERS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// A slot in [`LIVE_CLIPBOARD_READERS`], released when the thread ends.
struct ClipboardReader;

impl ClipboardReader {
    fn acquire() -> Option<Self> {
        if LIVE_CLIPBOARD_READERS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1
            > MAX_CLIPBOARD_READERS
        {
            LIVE_CLIPBOARD_READERS.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            return None;
        }
        Some(Self)
    }
}

impl Drop for ClipboardReader {
    fn drop(&mut self) {
        LIVE_CLIPBOARD_READERS.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// What a reader thread has to know besides the pipe it is draining.
///
/// The data is what the write is for; this is the rest: which session and
/// serial it answers, the table saying whether it is still the write that owns
/// the selection, and the bus object needed to tell the frontend when
/// ownership has come back to the local side.
struct ClipboardReaderContext {
    sessions: Sessions,
    transfers: Arc<Mutex<Transfers>>,
    path: OwnedObjectPath,
    serial: u32,
    server: zbus::ObjectServer,
}

/// Read what the application writes into a `SelectionWrite` pipe, and record
/// it as the local selection.
///
/// On its own thread because the pipe blocks. The read end is non-blocking and
/// the thread gives up on a peer that goes quiet or never finishes, so a pipe
/// whose other end is held open and empty does not park a thread for the life
/// of the session; [`ClipboardReader`] bounds how many can be parked at once.
/// Whichever way it ends — EOF, timeout, or a read that fails — the transfer is
/// finished, because an ownership flag that outlives its write is one the
/// session could use to overwrite a later local copy.
fn read_clipboard_pipe(
    read: std::os::fd::OwnedFd,
    sender: smithay::reexports::calloop::channel::Sender<Message>,
    context: ClipboardReaderContext,
) {
    let text = read_clipboard(read);
    finish_clipboard_transfer(&context, text, &sender);
}

/// Drain the selection pipe, or answer `None` when it cannot be read at all.
fn read_clipboard(read: std::os::fd::OwnedFd) -> Option<String> {
    use smithay::reexports::rustix::fs::{fcntl_setfl, OFlags};
    use std::io::Read as _;

    if fcntl_setfl(&read, OFlags::NONBLOCK).is_err() {
        return None;
    }
    let mut file = std::fs::File::from(read);
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8192];
    let started = std::time::Instant::now();
    let mut last = started;
    let mut readable = true;
    loop {
        match file.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buffer.extend_from_slice(&chunk[..n]);
                if buffer.len() >= crate::clipboard::MAX_BYTES {
                    buffer.truncate(crate::clipboard::MAX_BYTES);
                    break;
                }
                last = std::time::Instant::now();
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if last.elapsed() >= CLIPBOARD_IDLE || started.elapsed() >= CLIPBOARD_TOTAL {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(_) => {
                readable = false;
                break;
            }
        }
    }
    readable.then(|| String::from_utf8_lossy(&buffer).into_owned())
}

/// Finish one transfer on the reader thread.
///
/// Ownership goes back to the local side the moment the write ends, whether
/// bytes arrived or the reader gave up, and the frontend is told. A transfer
/// that a newer `SetSelection` has already replaced records nothing: the new
/// selection is what owns the clipboard now, and the old pipe is a stale
/// answer to a question nobody is asking any more.
fn finish_clipboard_transfer(
    context: &ClipboardReaderContext,
    text: Option<String>,
    sender: &smithay::reexports::calloop::channel::Sender<Message>,
) {
    let owner_returned = {
        // Session table first, then transfers, in that order everywhere both
        // are held. That is what makes "still the owner" one answer rather
        // than a race between the two maps.
        let mut shared = context.sessions.lock().unwrap();
        let current = context
            .transfers
            .lock()
            .unwrap()
            .completed(&context.path, context.serial);
        match shared.sessions.get_mut(&context.path) {
            Some(session) if current && session.clipboard_owner => {
                session.clipboard_owner = false;
                true
            }
            _ => false,
        }
    };
    if !owner_returned {
        return;
    }
    if let Some(text) = text {
        let _ = sender.send(Message::ClipboardSet { text });
    }
    // The local side owns the selection now, so the frontend is told. This is
    // also what lets a local paste reach the other end rather than serving the
    // remote copy the session put here.
    zbus::block_on(emit_owner_changed(
        &context.server,
        &context.path,
        &crate::clipboard::offered_mimes(),
        false,
    ));
}

/// One local selection change on its way to the portal object on the bus.
struct LocalSelectionChange {
    /// The sessions whose claim this change cleared, and which have to be
    /// told the local side owns the selection now.
    sessions: Vec<OwnedObjectPath>,
    /// What the local selection offers, handed to the frontend so the remote
    /// application has something to paste.
    mimes: Vec<String>,
}

/// The way back from the compositor thread to the `Clipboard` object.
///
/// `ViewportState` has no handle to that object: it is built and served by the
/// appearance process, on a connection the compositor state never sees. But a
/// local copy or a history paste has to tell every remote session that was
/// holding the clipboard that it no longer holds it. The sessions and
/// transfers are shared with the object, and the object server is captured
/// from the first bus call that can hand out ownership; a watcher thread does
/// the signalling, so a D-Bus write is never on the compositor's path.
struct ClipboardBridge {
    sessions: Sessions,
    transfers: Arc<Mutex<Transfers>>,
    /// The bus object, known only once an interface method has run. It is
    /// captured before a session can be made an owner, so there is always a
    /// way to tell that owner the selection has gone.
    server: Mutex<Option<zbus::ObjectServer>>,
    /// Where a local change waits for the watcher.
    changes: std::sync::mpsc::Sender<LocalSelectionChange>,
}

/// The one `Clipboard` object this process serves.
static CLIPBOARD_BRIDGE: std::sync::OnceLock<ClipboardBridge> = std::sync::OnceLock::new();

/// Start the watcher that takes local ownership changes to the bus.
///
/// One thread for the life of the process, parked on the channel. It exists
/// because `ViewportState` cannot emit a portal signal itself, and the
/// compositor thread must not wait on D-Bus to announce a copy.
fn start_local_selection_watch(sessions: &Sessions, transfers: &Arc<Mutex<Transfers>>) {
    if CLIPBOARD_BRIDGE.get().is_some() {
        return;
    }
    let (changes, receiver) = std::sync::mpsc::channel();
    let bridge = ClipboardBridge {
        sessions: sessions.clone(),
        transfers: transfers.clone(),
        server: Mutex::new(None),
        changes,
    };
    if CLIPBOARD_BRIDGE.set(bridge).is_err() {
        return;
    }
    if let Err(e) = std::thread::Builder::new()
        .name("clipboard-owner".to_owned())
        .spawn(move || watch_local_selection(receiver))
    {
        // The ownership flags still clear; only the frontend misses the
        // announcement, which is worth a line rather than silence.
        tracing::warn!("clipboard: could not start the local-selection watcher: {e}");
    }
}

/// Emit `SelectionOwnerChanged(false)` for each session a local selection took
/// the clipboard from.
fn watch_local_selection(changes: std::sync::mpsc::Receiver<LocalSelectionChange>) {
    let Some(bridge) = CLIPBOARD_BRIDGE.get() else {
        return;
    };
    for change in changes {
        let Some(server) = bridge.server.lock().unwrap().clone() else {
            // An owner cannot exist before the bus object has answered a call,
            // so this only happens while the connection is going away.
            tracing::warn!("clipboard: no bus object to announce the local selection");
            continue;
        };
        for session in change.sessions {
            // The session may have claimed the selection again while this
            // change waited for the bus, or may have closed. In either case
            // the false this change carries is no longer the truth about the
            // session, and emitting it would tell the frontend the opposite
            // of what `SetSelection` just said.
            if !selection_still_local(&bridge.sessions, &session) {
                continue;
            }
            zbus::block_on(emit_owner_changed(&server, &session, &change.mimes, false));
        }
    }
}

/// Whether `session` still has no claim on the clipboard.
///
/// The watcher asks this just before sending the false ownership, because a
/// later `SetSelection` can win a race against a local copy that has already
/// cleared the flag. A session that is gone answers false: there is nobody to
/// tell.
fn selection_still_local(sessions: &Sessions, session: &OwnedObjectPath) -> bool {
    sessions
        .lock()
        .unwrap()
        .sessions
        .get(session)
        .is_some_and(|row| !row.clipboard_owner)
}

/// A local selection has replaced whatever a remote session had claimed.
///
/// Called from the compositor thread when a Wayland client copies, or when the
/// compositor offers an entry out of the history. Every session that still
/// claimed the clipboard is marked as not owning it, under the same sessions
/// lock `SetSelection` takes, and its outstanding transfers are dropped so a
/// serial minted before this copy cannot be answered after it. The frontend is
/// told from a watcher thread, so this never blocks the event loop on D-Bus.
///
/// A cheap no-op when no `Clipboard` object was ever built — no session can
/// have been granted the clipboard through it — and when no session still
/// claims the selection.
pub(crate) fn local_selection_changed(mimes: Vec<String>) {
    let Some(bridge) = CLIPBOARD_BRIDGE.get() else {
        return;
    };
    let sessions = take_remote_ownership(&bridge.sessions, &bridge.transfers);
    if sessions.is_empty() {
        return;
    }
    if bridge
        .changes
        .send(LocalSelectionChange { sessions, mimes })
        .is_err()
    {
        // The watcher only goes away with the process or when its thread could
        // not start; there is nobody left to tell.
        tracing::debug!("clipboard: no watcher for local selection changes");
    }
}

/// Take the selection back from every remote session that still claims it.
///
/// The sessions that claimed it are returned so the caller can announce it to
/// each one. Their transfers go with the claim: a serial the remote side was
/// given before the local copy is not an answer to anything any more, even if
/// the session later re-advertises a type this end cannot read and so mints no
/// replacement serial.
fn take_remote_ownership(
    sessions: &Sessions,
    transfers: &Mutex<Transfers>,
) -> Vec<OwnedObjectPath> {
    // Sessions and then transfers, held together, in the order every other
    // path takes them. Clearing the claim and dropping its transfer in one
    // critical section is what stops a `SetSelection` arriving in between
    // from being treated as neither owner nor claimant: it either happens
    // before, and its transfer is dropped with the claim, or after, and it
    // gets a fresh serial.
    let mut shared = sessions.lock().unwrap();
    let claimed = shared
        .sessions
        .iter_mut()
        .filter_map(|(path, session)| {
            session.clipboard_owner.then(|| {
                session.clipboard_owner = false;
                path.clone()
            })
        })
        .collect::<Vec<_>>();
    if claimed.is_empty() {
        return claimed;
    }
    let mut transfers = transfers.lock().unwrap();
    for path in &claimed {
        transfers.released(path);
    }
    claimed
}

/// The `org.freedesktop.impl.portal.Clipboard` object.
///
/// A session created by RemoteDesktop or InputCapture asks for clipboard
/// access here; this interface creates no sessions of its own. The data itself
/// is the compositor's clipboard history, so every method is a question for
/// the compositor thread or a message to it — the bus thread never touches the
/// selection, and a client that has the clipboard open does not have to be
/// reachable from here.
///
/// Text only, like the history: an image or a file list has nowhere to be read
/// back from, and the remote-desktop protocol has a separate file-transfer
/// portal for files.
pub struct Clipboard {
    sender: smithay::reexports::calloop::channel::Sender<Message>,
    sessions: Sessions,
    /// The transfers this end has asked for, by serial. Shared with the
    /// reader threads, which use it to tell whether what they read is still
    /// the selection that owns the clipboard.
    transfers: Arc<Mutex<Transfers>>,
}

impl Clipboard {
    pub fn new(
        sender: smithay::reexports::calloop::channel::Sender<Message>,
        sessions: Sessions,
    ) -> Self {
        let transfers = Arc::new(Mutex::new(Transfers::default()));
        start_local_selection_watch(&sessions, &transfers);
        Self {
            sender,
            sessions,
            transfers,
        }
    }

    /// Remember the bus object from a live interface call.
    ///
    /// The server reaches `SetSelection` and `SelectionWrite` as an argument
    /// and nowhere else; a later local selection needs it to emit an ownership
    /// change, so the first call that is in a position to grant ownership
    /// leaves it here for the watcher thread. Before then no session can own
    /// the selection, so not having it is not a gap.
    fn remember_bus(&self, server: &zbus::ObjectServer) {
        if let Some(bridge) = CLIPBOARD_BRIDGE.get() {
            *bridge.server.lock().unwrap() = Some(server.clone());
        }
    }

    fn called_by_frontend(&self, header: &zbus::message::Header<'_>) -> bool {
        super::portal::called_by_frontend(&self.sessions, "clipboard", header)
    }

    /// Whether Start granted this session the clipboard.
    ///
    /// The request flag is not the answer: the application was told what
    /// `clipboard_enabled` said at Start, and only that may gate the
    /// selection. See `Session::clipboard_granted`.
    fn granted(&self, path: &OwnedObjectPath) -> bool {
        self.sessions
            .lock()
            .unwrap()
            .sessions
            .get(path)
            .is_some_and(|session| session.clipboard_granted)
    }
}

#[zbus::interface(name = "org.freedesktop.impl.portal.Clipboard")]
impl Clipboard {
    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        1
    }

    /// The application wants the clipboard for this session.
    ///
    /// Called before Start; the answer is the `clipboard_enabled` field of
    /// that call's results, not this one's — the interface defines it as
    /// one-way.
    async fn request_clipboard(
        &self,
        session_handle: ObjectPath<'_>,
        _options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) {
        if !self.called_by_frontend(&header) {
            return;
        }
        let path = OwnedObjectPath::from(session_handle);
        if let Some(session) = self.sessions.lock().unwrap().sessions.get_mut(&path) {
            if session.ask_for_clipboard() {
                tracing::debug!("clipboard: {path} asked for access");
            } else {
                // Late, or a session that has no clipboard to ask for. Either
                // way the application was already answered; changing the flag
                // now would contradict that answer.
                tracing::debug!("clipboard: ignoring a clipboard request from {path}");
            }
        }
    }

    /// The remote session now owns the clipboard, offering `mime_types`.
    ///
    /// The data itself arrives through `SelectionWrite`; this is the
    /// advertisement, and the `SelectionOwnerChanged` signal is what tells the
    /// frontend to read it back when the local side pastes.
    async fn set_selection(
        &self,
        session_handle: ObjectPath<'_>,
        options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(object_server)] server: &zbus::ObjectServer,
    ) {
        if !self.called_by_frontend(&header) {
            return;
        }
        self.remember_bus(server);
        let path = OwnedObjectPath::from(session_handle);
        let mimes = mime_types(&options);
        let wanted = transfer_mime(&mimes);

        // The session table and the transfer table are taken together, in
        // that order, so what the session says it owns and what the transfer
        // table thinks it owns cannot be read half-way through each other.
        // Both are dropped before anything is awaited.
        let (stored, transfer, released) = {
            let mut shared = self.sessions.lock().unwrap();
            let Some(session) = shared.sessions.get_mut(&path) else {
                return;
            };
            if !session.clipboard_granted {
                return;
            }
            let mut transfers = self.transfers.lock().unwrap();
            if mimes.is_empty() {
                // A session advertising no types is giving the selection up,
                // not claiming one with nothing to offer. The local side has
                // the selection again and is told so before anything can ask
                // for a transfer that no longer exists.
                let was_owner = session.clipboard_owner;
                session.clipboard_owner = false;
                session.clipboard_mimes.clear();
                let had_transfer = transfers.released(&path);
                (None, None, was_owner || had_transfer)
            } else {
                session.clipboard_mimes = mimes.clone();
                session.clipboard_owner = true;
                match wanted {
                    Some(mime) => {
                        // One transfer at a time per session: the serial is
                        // what `SelectionWrite` has to name, and a newer
                        // advertisement makes the previous one stale.
                        let serial = next_transfer_serial();
                        transfers.requested(&path, serial, std::time::Instant::now());
                        (Some(mimes), Some((mime, serial)), false)
                    }
                    // Nothing this end can read — an image, a file list.
                    // The session owns the selection, but there is no
                    // transfer to ask for and no serial to answer with.
                    None => (Some(mimes), None, false),
                }
            }
        };

        if released {
            tracing::debug!("clipboard: {path} released the selection");
            emit_owner_changed(server, &path, &[], false).await;
            return;
        }
        let Some(stored) = stored else {
            return;
        };
        tracing::debug!("clipboard: {path} now owns {stored:?}");
        emit_owner_changed(server, &path, &stored, true).await;

        // And ask for the bytes. A session offering a type this side cannot
        // read is told nothing further; one offering text is asked for it, and
        // answers with `SelectionWrite`.
        if let Some((mime, serial)) = transfer {
            emit_selection_transfer(server, &path, &mime, serial).await;
        }
    }

    /// The application is pasting; hand it what the local selection holds.
    async fn selection_read(
        &self,
        session_handle: ObjectPath<'_>,
        _mime_type: &str,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<zvariant::OwnedFd> {
        if !self.called_by_frontend(&header) {
            return Err(zbus::fdo::Error::AccessDenied(
                "that is not the portal frontend".to_owned(),
            ));
        }
        let path = OwnedObjectPath::from(session_handle);
        if !self.granted(&path) {
            return Err(zbus::fdo::Error::AccessDenied(
                "this session was not granted the clipboard".to_owned(),
            ));
        }

        // The selection lives on the compositor thread, so it is asked for
        // rather than read.
        let (sender, receiver) = async_channel::bounded(1);
        self.sender
            .send(Message::ClipboardRead { reply: sender })
            .map_err(|_| zbus::fdo::Error::Failed("the compositor is not listening".to_owned()))?;
        let text = receiver
            .recv()
            .await
            .map_err(|_| zbus::fdo::Error::Failed("the compositor did not answer".to_owned()))?
            .unwrap_or_default();

        // A pipe the application reads: this end is written on a thread,
        // because the application may not read it for as long as it likes and
        // this is the bus connection the rest of the desktop shares. The
        // thread and its fd are bounded by the clipboard's own writer cap,
        // which `serve` owns.
        let (read, write) = smithay::reexports::rustix::pipe::pipe()
            .map_err(|e| zbus::fdo::Error::Failed(format!("no pipe for the clipboard: {e}")))?;
        crate::clipboard::serve(text, write);
        Ok(zvariant::OwnedFd::from(read))
    }

    /// The application is answering a `SelectionTransfer`; hand it a pipe to
    /// write the data into.
    ///
    /// The serial is not decoration: it has to name the outstanding transfer
    /// for this same session, because an answer to a question this end never
    /// asked — or asked and has since replaced — is how a granted session
    /// could write over a newer local selection at any time.
    async fn selection_write(
        &self,
        session_handle: ObjectPath<'_>,
        serial: u32,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(object_server)] server: &zbus::ObjectServer,
    ) -> zbus::fdo::Result<zvariant::OwnedFd> {
        if !self.called_by_frontend(&header) {
            return Err(zbus::fdo::Error::AccessDenied(
                "that is not the portal frontend".to_owned(),
            ));
        }
        self.remember_bus(server);
        let path = OwnedObjectPath::from(session_handle);
        if !self.granted(&path) {
            return Err(zbus::fdo::Error::AccessDenied(
                "this session was not granted the clipboard".to_owned(),
            ));
        }

        // Taken before the transfer is spent, so a full reader pool refuses
        // the write with the serial still outstanding and answerable later.
        let Some(reader) = ClipboardReader::acquire() else {
            tracing::warn!("clipboard: too many remote reads in flight; refusing a transfer");
            return Err(zbus::fdo::Error::Failed(
                "too many clipboard transfers in flight".to_owned(),
            ));
        };
        let (read, write) = smithay::reexports::rustix::pipe::pipe()
            .map_err(|e| zbus::fdo::Error::Failed(format!("no pipe for the clipboard: {e}")))?;

        let answer = {
            let mut shared = self.sessions.lock().unwrap();
            let Some(session) = shared.sessions.get_mut(&path) else {
                return Err(zbus::fdo::Error::AccessDenied(
                    "this session was not granted the clipboard".to_owned(),
                ));
            };
            if !session.clipboard_granted {
                return Err(zbus::fdo::Error::AccessDenied(
                    "this session was not granted the clipboard".to_owned(),
                ));
            }
            if !session.clipboard_owner {
                return Err(zbus::fdo::Error::Failed(
                    "this session does not own the clipboard".to_owned(),
                ));
            }
            let answer = self.transfers.lock().unwrap().answered(
                &path,
                serial,
                std::time::Instant::now(),
                CLIPBOARD_TRANSFER,
            );
            if answer == TransferAnswer::Expired {
                // The offer aged out while it sat unanswered, so the local
                // side has the selection again. Say so before refusing, and
                // the stale serial cannot be used to overwrite it.
                session.clipboard_owner = false;
            }
            answer
        };

        match answer {
            TransferAnswer::Ready => {}
            TransferAnswer::Expired => {
                emit_owner_changed(server, &path, &[], false).await;
                return Err(zbus::fdo::Error::Failed(
                    "that clipboard transfer has expired".to_owned(),
                ));
            }
            TransferAnswer::Unknown => {
                return Err(zbus::fdo::Error::Failed(
                    "no clipboard transfer with that serial".to_owned(),
                ));
            }
        }

        let context = ClipboardReaderContext {
            sessions: self.sessions.clone(),
            transfers: self.transfers.clone(),
            path,
            serial,
            server: server.clone(),
        };
        let sender = self.sender.clone();
        if let Err(e) = std::thread::Builder::new()
            .name("clipboard-write".to_owned())
            .spawn(move || {
                let _reader = reader;
                read_clipboard_pipe(read, sender, context);
            })
        {
            tracing::warn!("clipboard: could not start a reader: {e}");
            return Err(zbus::fdo::Error::Failed(
                "could not start a reader".to_owned(),
            ));
        }
        Ok(zvariant::OwnedFd::from(write))
    }

    /// The application has finished writing the selection, or failed to.
    ///
    /// The reader thread `SelectionWrite` started is what records the data:
    /// closing the write end is the EOF it waits for, and `success` false only
    /// means the bytes are whatever arrived before it gave up.
    async fn selection_write_done(
        &self,
        session_handle: ObjectPath<'_>,
        serial: u32,
        success: bool,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) {
        if !self.called_by_frontend(&header) {
            return;
        }
        let path = OwnedObjectPath::from(session_handle);
        if !self.granted(&path) {
            return;
        }
        // The reader thread is what records the bytes and hands the selection
        // back to the local side, on EOF or on its own deadline. What is
        // checked here is only that the serial names a write this end actually
        // handed out, so a completion cannot be aimed at a transfer that never
        // existed.
        if self.transfers.lock().unwrap().written_by(&path, serial) {
            tracing::debug!(
                "clipboard: {path} finished writing transfer {serial} (success={success})"
            );
        } else {
            tracing::debug!(
                "clipboard: {path} reported transfer {serial} that is not being written"
            );
        }
    }

    /// The local selection changed, and the session may now offer it.
    #[zbus(signal)]
    async fn selection_owner_changed(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        session_handle: ObjectPath<'_>,
        options: HashMap<String, Value<'_>>,
    ) -> zbus::Result<()>;

    /// Ask the frontend to transfer a remote session's selection to us.
    ///
    /// Emitted after `SetSelection` announces ownership: the frontend answers
    /// by calling `SelectionWrite`, whose reader records the data as the local
    /// selection. See [`emit_selection_transfer`].
    #[zbus(signal)]
    async fn selection_transfer(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        session_handle: ObjectPath<'_>,
        mime_type: &str,
        serial: u32,
    ) -> zbus::Result<()>;
}

fn mime_types(options: &HashMap<String, OwnedValue>) -> Vec<String> {
    options
        .get("mime_types")
        .and_then(|value| <Vec<String>>::try_from(value.clone()).ok())
        .unwrap_or_default()
}

/// The serial the next `SelectionTransfer` carries.
///
/// The frontend echoes it back in `SelectionWrite`, so it has to be unique per
/// transfer. Zero is skipped because it is the value a caller might read as
/// unset.
static TRANSFER_SERIAL: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

fn next_transfer_serial() -> u32 {
    loop {
        let serial = TRANSFER_SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if serial != 0 {
            return serial;
        }
    }
}

/// Which of a session's offered types this compositor can actually read.
///
/// Text only, like the history: an image or a file list has nowhere to be read
/// back from here, and the remote-desktop protocol has a separate file-transfer
/// portal for files.
fn transfer_mime(mimes: &[String]) -> Option<String> {
    crate::clipboard::Clipboard::text_mime(mimes)
}

/// Ask the frontend for a remote session's selection.
///
/// `SetSelection` is only the advertisement; the frontend answers this by
/// calling `SelectionWrite`, and nothing else ever pulls the remote's data into
/// the local clipboard. Without it the signal was declared and never sent, so a
/// remote copy never reached a local paste.
async fn emit_selection_transfer(
    server: &zbus::ObjectServer,
    session: &OwnedObjectPath,
    mime: &str,
    serial: u32,
) {
    let Ok(interface) = server.interface::<_, Clipboard>(OBJECT_PATH).await else {
        return;
    };
    if let Err(e) =
        Clipboard::selection_transfer(interface.signal_emitter(), session.as_ref(), mime, serial)
            .await
    {
        tracing::warn!("clipboard: could not emit SelectionTransfer: {e}");
    }
}

async fn emit_owner_changed(
    server: &zbus::ObjectServer,
    session: &OwnedObjectPath,
    mimes: &[String],
    owner: bool,
) {
    let Ok(interface) = server.interface::<_, Clipboard>(OBJECT_PATH).await else {
        return;
    };
    let mut options: HashMap<String, Value<'_>> = HashMap::new();
    options.insert("mime_types".to_owned(), Value::from(mimes.to_vec()));
    options.insert("session_is_owner".to_owned(), Value::from(owner));
    if let Err(e) =
        Clipboard::selection_owner_changed(interface.signal_emitter(), session.as_ref(), options)
            .await
    {
        tracing::warn!("clipboard: could not emit SelectionOwnerChanged: {e}");
    }
}

/// How far a discrete scroll of this many notches goes, on each axis.
///
/// Split out from the handler so the arithmetic can be tested without a seat:
/// the sign convention is the part that is easy to get backwards, and getting
/// it backwards is a remote session that scrolls the wrong way with nothing in
/// any log to say so.
///
/// The answer is `(horizontal, vertical)` in `wl_pointer.axis` units, and the
/// v120 value a client reads is the notch count times 120 — which is what the
/// name means: one detent is 120, and a high-resolution wheel sends fractions
/// of it.
pub fn discrete_axis(axis: u32, steps: i32) -> (f64, f64) {
    let distance = steps as f64 * NOTCH;
    match axis {
        AXIS_HORIZONTAL => (distance, 0.0),
        // Vertical is zero, and so is anything the interface grows later: a
        // third axis nobody here knows about must not be silently scrolled as
        // if it were the wheel.
        AXIS_VERTICAL => (0.0, distance),
        _ => (0.0, 0.0),
    }
}

/// The v120 value for a notch count, which is what a client reads to know a
/// wheel turned rather than a touchpad moved.
pub fn discrete_v120(steps: i32) -> i32 {
    steps.saturating_mul(120)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_advertised_mime_types_come_out_of_the_options() {
        // `SetSelection` puts them under `mime_types`, as an array of strings.
        // Anything else — absent, the wrong type — is a session offering
        // nothing, which is not an error and not worth refusing the call over.
        let mut options: HashMap<String, OwnedValue> = HashMap::new();
        options.insert(
            "mime_types".to_owned(),
            Value::from(vec![
                "text/plain;charset=utf-8".to_owned(),
                "UTF8_STRING".to_owned(),
            ])
            .try_to_owned()
            .expect("a string array holds no file descriptor"),
        );
        assert_eq!(
            mime_types(&options),
            vec!["text/plain;charset=utf-8", "UTF8_STRING"]
        );
        assert!(mime_types(&HashMap::new()).is_empty());

        let mut wrong: HashMap<String, OwnedValue> = HashMap::new();
        wrong.insert("mime_types".to_owned(), OwnedValue::from(7u32));
        assert!(mime_types(&wrong).is_empty());
    }

    /// What a `SelectionTransfer` is sent for, and with.
    ///
    /// The signal exists to pull the remote's bytes into the local clipboard,
    /// and only text can be read back here; the serial has to be unique and
    /// never zero, because the frontend echoes it back in `SelectionWrite`.
    #[test]
    fn a_transfer_asks_for_a_text_type_with_a_fresh_serial() {
        assert_eq!(
            transfer_mime(&["image/png".to_owned(), "text/plain".to_owned()]).as_deref(),
            Some("text/plain")
        );
        assert_eq!(transfer_mime(&["image/png".to_owned()]), None);
        assert_eq!(transfer_mime(&[]), None);

        let first = next_transfer_serial();
        let second = next_transfer_serial();
        assert_ne!(first, 0, "zero reads as unset to a caller");
        assert_ne!(first, second);
    }

    /// Every event names the device it would have come from. One that named
    /// the wrong one would be a session granted a mouse and allowed to type,
    /// which is the whole of what the grant is for.
    #[test]
    fn every_injection_names_its_device() {
        let cases = [
            (
                Injection::PointerMotion { dx: 1.0, dy: 0.0 },
                DEVICE_POINTER,
            ),
            (
                Injection::PointerMotionAbsolute {
                    stream: 1,
                    x: 0.0,
                    y: 0.0,
                },
                DEVICE_POINTER,
            ),
            (
                Injection::PointerButton {
                    button: 0x110,
                    pressed: true,
                },
                DEVICE_POINTER,
            ),
            (
                Injection::PointerAxis {
                    dx: 0.0,
                    dy: 15.0,
                    finish: false,
                },
                DEVICE_POINTER,
            ),
            (
                Injection::PointerAxisDiscrete { axis: 0, steps: 1 },
                DEVICE_POINTER,
            ),
            (
                Injection::KeyboardKeycode {
                    keycode: 30,
                    pressed: true,
                },
                DEVICE_KEYBOARD,
            ),
            (
                Injection::KeyboardKeysym {
                    keysym: 0x61,
                    pressed: true,
                },
                DEVICE_KEYBOARD,
            ),
            (
                Injection::TouchDown {
                    stream: 1,
                    slot: 0,
                    x: 0.0,
                    y: 0.0,
                },
                DEVICE_TOUCHSCREEN,
            ),
            (
                Injection::TouchMotion {
                    stream: 1,
                    slot: 0,
                    x: 0.0,
                    y: 0.0,
                },
                DEVICE_TOUCHSCREEN,
            ),
            (Injection::TouchUp { slot: 0 }, DEVICE_TOUCHSCREEN),
        ];
        for (injection, device) in cases {
            assert_eq!(injection.device(), device, "{injection:?}");
        }
    }

    /// The device set is described in words the person being asked can read,
    /// in a fixed order. A chooser that said "1, 2" would be asking somebody
    /// to agree to a number.
    #[test]
    fn the_devices_are_named_in_a_fixed_order() {
        assert_eq!(
            device_names(ALL_DEVICES),
            ["keyboard", "mouse", "touchscreen"]
        );
        assert_eq!(device_names(DEVICE_POINTER), ["mouse"]);
        assert_eq!(
            device_names(DEVICE_TOUCHSCREEN | DEVICE_KEYBOARD),
            ["keyboard", "touchscreen"]
        );
        assert!(device_names(0).is_empty());
    }

    /// A bit from a later version of the interface names nothing, which is
    /// what keeps the chooser honest: it must describe exactly the permission
    /// that `select_devices` masked the request down to.
    #[test]
    fn an_unknown_device_is_not_described() {
        assert_eq!(
            device_names(ALL_DEVICES | 0x8000),
            device_names(ALL_DEVICES)
        );
    }

    /// A wheel notch goes the way it was asked to, on the axis it was asked
    /// for. Backwards here is a remote session that scrolls up when the other
    /// end scrolls down, which nothing in any log would explain.
    #[test]
    fn a_notch_scrolls_its_own_axis() {
        assert_eq!(discrete_axis(AXIS_VERTICAL, 1), (0.0, NOTCH));
        assert_eq!(discrete_axis(AXIS_VERTICAL, -2), (0.0, -2.0 * NOTCH));
        assert_eq!(discrete_axis(AXIS_HORIZONTAL, 1), (NOTCH, 0.0));
        assert_eq!(discrete_axis(9, 1), (0.0, 0.0));
    }

    /// One detent is 120, which is what a client divides by to count notches.
    #[test]
    fn a_notch_is_a_hundred_and_twenty() {
        assert_eq!(discrete_v120(1), 120);
        assert_eq!(discrete_v120(-3), -360);
        // And an absurd count saturates rather than wrapping into a scroll in
        // the other direction. The number comes off the bus.
        assert_eq!(discrete_v120(i32::MAX), i32::MAX);
    }
    /// Every event carrying a coordinate refuses NaN and infinities, and every
    /// event without one passes. A NaN that reached the seat would become the
    /// pointer's location and stick, so this is the only line of defence.
    #[test]
    fn non_finite_coordinates_are_not_input() {
        let nan = f64::NAN;
        let inf = f64::INFINITY;
        for injection in [
            Injection::PointerMotion { dx: nan, dy: 0.0 },
            Injection::PointerMotion { dx: 0.0, dy: inf },
            Injection::PointerMotionAbsolute {
                stream: 1,
                x: f64::NEG_INFINITY,
                y: 0.0,
            },
            Injection::PointerAxis {
                dx: nan,
                dy: 0.0,
                finish: false,
            },
            Injection::TouchDown {
                stream: 1,
                slot: 0,
                x: 0.0,
                y: nan,
            },
            Injection::TouchMotion {
                stream: 1,
                slot: 0,
                x: inf,
                y: 0.0,
            },
        ] {
            assert!(!injection.is_finite(), "{injection:?}");
        }
        for injection in [
            Injection::PointerMotion { dx: -3.5, dy: 2.25 },
            Injection::PointerMotionAbsolute {
                stream: 1,
                x: 640.0,
                y: 480.0,
            },
            Injection::PointerButton {
                button: 0x110,
                pressed: true,
            },
            Injection::PointerAxis {
                dx: 0.0,
                dy: 15.0,
                finish: true,
            },
            Injection::PointerAxisDiscrete { axis: 0, steps: -2 },
            Injection::KeyboardKeycode {
                keycode: 30,
                pressed: true,
            },
            Injection::KeyboardKeysym {
                keysym: 0x61,
                pressed: false,
            },
            Injection::TouchDown {
                stream: 1,
                slot: 0,
                x: 1.0,
                y: 2.0,
            },
            Injection::TouchMotion {
                stream: 1,
                slot: 0,
                x: 3.0,
                y: 4.0,
            },
            Injection::TouchUp { slot: 0 },
        ] {
            assert!(injection.is_finite(), "{injection:?}");
        }
    }
    /// The remote clipboard keeps the same ceiling the local one does: a peer
    /// that starts transfers and never finishes them uses a fixed number of
    /// threads and pipe fds, not one per D-Bus call.
    #[test]
    fn remote_clipboard_readers_are_capped() {
        let mut held = Vec::new();
        for _ in 0..MAX_CLIPBOARD_READERS {
            held.push(ClipboardReader::acquire().expect("under the cap"));
        }
        assert!(
            ClipboardReader::acquire().is_none(),
            "the cap must be a ceiling"
        );
        held.pop();
        assert!(
            ClipboardReader::acquire().is_some(),
            "a finished reader frees its slot"
        );
    }

    /// A transfer belongs to the serial it was sent under, only the session it
    /// was sent to can answer it, and it can be answered once. Without this
    /// the serial was decorative and any granted session could write into the
    /// local selection at any time.
    #[test]
    fn a_selection_transfer_is_answered_once_by_its_own_session() {
        let mut transfers = Transfers::default();
        let one = OwnedObjectPath::try_from("/org/freedesktop/portal/desktop/one")
            .expect("a valid object path");
        let two = OwnedObjectPath::try_from("/org/freedesktop/portal/desktop/two")
            .expect("a valid object path");
        let start = std::time::Instant::now();
        transfers.requested(&one, 7, start);

        assert_eq!(
            transfers.answered(&two, 7, start, CLIPBOARD_TRANSFER),
            TransferAnswer::Unknown,
            "another session's serial is not this session's to answer"
        );
        assert_eq!(
            transfers.answered(&one, 7, start, CLIPBOARD_TRANSFER),
            TransferAnswer::Ready
        );
        assert!(transfers.written_by(&one, 7));
        assert_eq!(
            transfers.answered(&one, 7, start, CLIPBOARD_TRANSFER),
            TransferAnswer::Unknown,
            "a serial answers one write and no more"
        );
        assert!(
            transfers.completed(&one, 7),
            "the write owned the selection"
        );
        assert!(!transfers.completed(&one, 7), "completion happens once");
    }

    /// A later advertisement replaces the earlier one, so the old pipe cannot
    /// record over the new selection — and cannot say the local side took a
    /// selection the session had already replaced.
    #[test]
    fn a_late_answer_cannot_overwrite_a_newer_selection() {
        let mut transfers = Transfers::default();
        let session = OwnedObjectPath::try_from("/org/freedesktop/portal/desktop/session")
            .expect("a valid object path");
        let start = std::time::Instant::now();
        transfers.requested(&session, 1, start);
        assert_eq!(
            transfers.answered(&session, 1, start, CLIPBOARD_TRANSFER),
            TransferAnswer::Ready
        );
        transfers.requested(&session, 2, start);

        assert!(
            !transfers.completed(&session, 1),
            "the superseded write is not the owner"
        );
        assert_eq!(
            transfers.answered(&session, 1, start, CLIPBOARD_TRANSFER),
            TransferAnswer::Unknown,
            "the superseded serial is gone"
        );
        assert_eq!(
            transfers.answered(&session, 2, start, CLIPBOARD_TRANSFER),
            TransferAnswer::Ready
        );
        assert!(
            transfers.completed(&session, 2),
            "the new write is the owner"
        );
    }

    /// An offer that waited too long is refused and gives the selection back;
    /// a release is once, not once per repeated empty advertisement.
    #[test]
    fn an_expired_or_released_transfer_gives_the_selection_back() {
        let mut transfers = Transfers::default();
        let session = OwnedObjectPath::try_from("/org/freedesktop/portal/desktop/session")
            .expect("a valid object path");
        let start = std::time::Instant::now();
        transfers.requested(&session, 3, start);
        let late = start + CLIPBOARD_TRANSFER + std::time::Duration::from_millis(1);
        assert_eq!(
            transfers.answered(&session, 3, late, CLIPBOARD_TRANSFER),
            TransferAnswer::Expired
        );
        assert_eq!(
            transfers.answered(&session, 3, start, CLIPBOARD_TRANSFER),
            TransferAnswer::Unknown,
            "an expired serial is gone"
        );

        transfers.requested(&session, 4, start);
        assert!(transfers.released(&session), "the session was the owner");
        assert!(!transfers.released(&session), "a release happens once");
        assert_eq!(
            transfers.answered(&session, 4, start, CLIPBOARD_TRANSFER),
            TransferAnswer::Unknown,
            "a released serial cannot answer"
        );
    }

    /// A session row claiming, or not claiming, the clipboard, for the tests
    /// of what a local copy does to a remote one.
    fn claimed_session(
        path: &str,
        owner: bool,
        mimes: &[&str],
    ) -> (OwnedObjectPath, crate::screencast::portal::Session) {
        let mut session = crate::screencast::portal::Session::default();
        session.clipboard_owner = owner;
        session.clipboard_mimes = mimes.iter().map(|mime| (*mime).to_owned()).collect();
        (
            OwnedObjectPath::try_from(path).expect("a valid object path"),
            session,
        )
    }

    /// A local copy takes the clipboard away from every remote session that
    /// claimed it, and leaves a session that did not alone.
    #[test]
    fn a_local_selection_takes_the_clipboard_back_from_every_owner() {
        let sessions = Sessions::default();
        let transfers = Mutex::new(Transfers::default());
        let (one, owner) =
            claimed_session("/org/freedesktop/portal/desktop/one", true, &["text/plain"]);
        let (two, quiet) = claimed_session(
            "/org/freedesktop/portal/desktop/two",
            false,
            &["text/plain"],
        );
        let (three, other) = claimed_session(
            "/org/freedesktop/portal/desktop/three",
            true,
            &["image/png"],
        );
        {
            let mut shared = sessions.lock().unwrap();
            shared.sessions.insert(one.clone(), owner);
            shared.sessions.insert(two.clone(), quiet);
            shared.sessions.insert(three.clone(), other);
        }

        let returned = take_remote_ownership(&sessions, &transfers);

        assert_eq!(returned.len(), 2);
        assert!(returned.contains(&one));
        assert!(returned.contains(&three));
        let shared = sessions.lock().unwrap();
        assert!(!shared.sessions.get(&one).unwrap().clipboard_owner);
        assert!(!shared.sessions.get(&three).unwrap().clipboard_owner);
        assert!(!shared.sessions.get(&two).unwrap().clipboard_owner);
        assert_eq!(
            shared.sessions.get(&two).unwrap().clipboard_mimes,
            vec!["text/plain"],
            "a session that was not an owner is untouched"
        );
    }

    /// A local copy with no remote owner has nothing to hand the bus.
    #[test]
    fn a_local_selection_with_no_remote_owner_hands_nothing_over() {
        let sessions = Sessions::default();
        let transfers = Mutex::new(Transfers::default());
        let (one, row) = claimed_session(
            "/org/freedesktop/portal/desktop/one",
            false,
            &["text/plain"],
        );
        sessions.lock().unwrap().sessions.insert(one.clone(), row);

        assert!(take_remote_ownership(&sessions, &transfers).is_empty());
        assert!(
            !sessions
                .lock()
                .unwrap()
                .sessions
                .get(&one)
                .unwrap()
                .clipboard_owner
        );
    }

    /// Taking the clipboard back is one event per claim: a second local
    /// selection has nothing new to tell the bus.
    #[test]
    fn the_remote_owner_is_told_once_per_claim() {
        let sessions = Sessions::default();
        let transfers = Mutex::new(Transfers::default());
        let (one, row) =
            claimed_session("/org/freedesktop/portal/desktop/one", true, &["text/plain"]);
        sessions.lock().unwrap().sessions.insert(one, row);

        assert_eq!(take_remote_ownership(&sessions, &transfers).len(), 1);
        assert!(take_remote_ownership(&sessions, &transfers).is_empty());
    }

    /// A transfer the remote side was given before the local copy cannot be
    /// answered after it, even if a later `SetSelection` with a type this end
    /// cannot read never mints a replacement serial.
    #[test]
    fn a_local_selection_drops_the_transfers_it_replaced() {
        let sessions = Sessions::default();
        let transfers = Mutex::new(Transfers::default());
        let (one, row) =
            claimed_session("/org/freedesktop/portal/desktop/one", true, &["text/plain"]);
        let start = std::time::Instant::now();
        transfers.lock().unwrap().requested(&one, 11, start);
        sessions.lock().unwrap().sessions.insert(one.clone(), row);

        assert_eq!(take_remote_ownership(&sessions, &transfers).len(), 1);
        assert_eq!(
            transfers
                .lock()
                .unwrap()
                .answered(&one, 11, start, CLIPBOARD_TRANSFER),
            TransferAnswer::Unknown,
            "the old serial is not an answer after the local copy"
        );
    }

    /// The watcher only says a session lost the clipboard if it still has no
    /// claim: a `SetSelection` that won the race, or a session that has
    /// closed, must not be told the local side owns anyway.
    #[test]
    fn a_session_that_claimed_again_is_not_told_the_local_side_won() {
        let sessions = Sessions::default();
        let (quiet, quiet_row) = claimed_session(
            "/org/freedesktop/portal/desktop/quiet",
            false,
            &["text/plain"],
        );
        let (claimed, claimed_row) = claimed_session(
            "/org/freedesktop/portal/desktop/claimed",
            true,
            &["text/plain"],
        );
        {
            let mut shared = sessions.lock().unwrap();
            shared.sessions.insert(quiet.clone(), quiet_row);
            shared.sessions.insert(claimed.clone(), claimed_row);
        }

        let gone = OwnedObjectPath::try_from("/org/freedesktop/portal/desktop/gone")
            .expect("a valid object path");
        assert!(selection_still_local(&sessions, &quiet));
        assert!(!selection_still_local(&sessions, &claimed));
        assert!(!selection_still_local(&sessions, &gone));
    }
}
