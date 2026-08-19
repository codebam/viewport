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

use zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

use super::portal::{Message, SessionObject, Sessions, Started as CastStarted};

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
        self.sessions.lock().unwrap().sessions.insert(
            path.clone(),
            super::portal::Session::new(app_id, owner, true),
        );

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
        let (devices, types) = {
            let shared = self.sessions.lock().unwrap();
            match shared.sessions.get(&path) {
                Some(session) => (
                    session.wanted_devices,
                    session.sources_selected.then_some(session.types),
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
        {
            let mut shared = self.sessions.lock().unwrap();
            let Some(session) = shared.sessions.get_mut(&path) else {
                // The frontend went away while the chooser was up. Nothing to
                // grant to, and nothing to stop either — the watcher has
                // already taken the row out.
                tracing::warn!("remote desktop: {path} was closed while it was being chosen");
                return (RESPONSE_CANCELLED, HashMap::new());
            };
            session.granted_devices = started.devices;
            session.node = started.cast.as_ref().map(|cast| cast.node);
        }
        tracing::info!(
            "remote desktop: {path} may drive the {}",
            device_names(started.devices).join(", ")
        );

        let mut results: HashMap<String, OwnedValue> = HashMap::new();
        results.insert("devices".to_owned(), OwnedValue::from(started.devices));
        // What the application may not do, said explicitly. The frontend reads
        // an absent key as false, but an application reading the results
        // dictionary sees the difference between "no" and "this compositor did
        // not think about it", and a remote session pasting into the machine
        // is the kind of thing that should be a stated no.
        results.insert("clipboard_enabled".to_owned(), OwnedValue::from(false));

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
}
