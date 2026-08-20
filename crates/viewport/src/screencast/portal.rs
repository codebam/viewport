// SPDX-License-Identifier: GPL-3.0-or-later
//
// org.freedesktop.impl.portal.ScreenCast.
//
// The interface xdg-desktop-portal calls when an application asks to share a
// screen. It normally answers to xdg-desktop-portal-wlr, which can only offer
// monitors — wlr-screencopy captures outputs and nothing else — so a browser
// asking for a window is handed a whole screen instead. Answering it here is
// what lets a window be offered: the compositor already composites them.
//
// The conversation is three calls. CreateSession makes a handle the frontend
// keeps, SelectSources says what kind of thing is wanted, and Start returns
// the PipeWire node to connect to. Each is answered with a response code and a
// dictionary, and each may be refused — a refusal is what the user cancelling
// looks like from here.
//
// It runs on the D-Bus thread and asks the compositor over a channel, because
// picking a window and compositing it belong where the windows are.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

use super::Remembered;

/// What the compositor is asked to do, and where to send the answer.
///
/// The reply travels back on a channel of its own: the D-Bus thread has to
/// answer the frontend synchronously, and the compositor is the only place
/// that knows what is on screen.
#[derive(Debug)]
pub enum Message {
    Start {
        /// What kind of source the client asked for, as the portal numbers
        /// them.
        types: u32,
        /// What was shared last time, if the application came back with a
        /// token for it. Only a wish: the monitor may be unplugged and the
        /// window closed, and the compositor asks the user when it cannot be
        /// honoured.
        restore: Option<Remembered>,
        reply: async_channel::Sender<Result<Started, String>>,
    },
    /// The same question for a remote-desktop session: may this application
    /// drive the machine, and — if it also asked to see it — what with.
    ///
    /// A separate variant rather than a flag on [`Message::Start`] because the
    /// answer is a different shape. A screen share hands back one stream; a
    /// remote-desktop session hands back the set of devices that were actually
    /// granted, and a stream only when the application asked for one too.
    StartRemote {
        /// What the application asked to be able to drive, as the interface's
        /// bitmask. Never more than this is granted, and the user may grant
        /// less.
        devices: u32,
        /// What kind of source the application also wants to see, if it called
        /// SelectSources on the same session. `None` is a session that drives
        /// the machine without watching it, which the interface allows and
        /// which is what a session set up purely to type into it looks like.
        types: Option<u32>,
        reply: async_channel::Sender<Result<super::remote::Started, String>>,
    },
    /// One input event from a session that was granted the device it names.
    ///
    /// Checked on the bus thread before it is sent — see
    /// [`super::remote::RemoteDesktop`] — so by the time it reaches the
    /// compositor the only question left is where on the desk it lands.
    Inject(super::remote::Injection),
    /// A libei socket for a session that was granted one, and what it was
    /// granted.
    ///
    /// The stream rather than the context: an `eis::Context` is read by a
    /// calloop source that owns it, and calloop belongs to the compositor
    /// thread. So the bus thread makes the pair, answers ConnectToEIS with one
    /// half, and sends the other half here to be built into a server. See
    /// [`crate::libei`] for the whole path, and
    /// [`super::remote::RemoteDesktop::connect_to_eis`] for the checks that
    /// happen before this message exists at all.
    ///
    /// `devices` travels with it because it is the grant: what the compositor
    /// does with it is create exactly those devices and no others, which is
    /// how consent is enforced on a connection nobody checks per event.
    ConnectEis {
        session: OwnedObjectPath,
        stream: std::os::unix::net::UnixStream,
        devices: u32,
    },
    /// A session with a libei socket has ended, so the socket must go.
    ///
    /// The Notify path needs nothing like this — the grant is checked per
    /// event, and an event for a session that is no longer in the table is
    /// dropped. A libei client is holding a socket instead, and a socket goes
    /// on working after the table forgets about it. Sent from both places a
    /// session can end: [`SessionObject::close`] and [`watch_frontend`].
    RevokeEis {
        session: OwnedObjectPath,
    },
    Close {
        node: u32,
    },
}

/// What a client needs to receive the stream.
#[derive(Debug, Clone)]
pub struct Started {
    pub node: u32,
    pub width: i32,
    pub height: i32,
    /// Which kind it turned out to be — a request for either is answered with
    /// whichever the user picked.
    pub source_type: u32,
    /// What was shared, written down for next time.
    ///
    /// Sent back whether or not this share was a restored one: the token the
    /// frontend hands the application is minted from this, and a share that
    /// answered with nothing is one the application can never ask for again.
    pub remembered: Option<Remembered>,
}

/// The response codes the portal interface uses.
const RESPONSE_SUCCESS: u32 = 0;
const RESPONSE_CANCELLED: u32 = 1;
const RESPONSE_FAILED: u32 = 2;

/// Who wrote a piece of restore data, and which shape it is in.
///
/// The frontend stores it and hands it back without looking inside, to
/// whichever implementation is running now. That may not be this one — a
/// session started under another compositor leaves a token behind that this
/// one would otherwise read as its own — so it is signed, and anything not
/// signed this way is ignored and the user is asked instead.
const RESTORE_VENDOR: &str = "viewport";
const RESTORE_VERSION: u32 = 2;

/// The bus name the portal frontend answers on.
///
/// Its owner is the only peer whose calls mean anything here. This interface is
/// on the session bus, which every process in the session can reach, and the
/// calls on it start a screen share — so a peer that is not the frontend is one
/// asking to record the desk on its own say-so.
const FRONTEND_NAME: &str = "org.freedesktop.portal.Desktop";

/// How many remembered sources are kept at once.
///
/// A ceiling rather than a policy: each is three short strings, and the table
/// is only ever added to by a share the user agreed to. What it stops is a
/// session left up for weeks accumulating one entry per share.
const REMEMBERED_LIMIT: usize = 32;

/// The application does not want the choice remembered.
const PERSIST_NONE: u32 = 0;

/// One conversation with an application.
///
/// Shared with the RemoteDesktop implementation next door rather than
/// duplicated for it, because the interfaces genuinely share a session: the
/// frontend calls CreateSession on RemoteDesktop, SelectSources on ScreenCast
/// with the *same* session handle, and Start on RemoteDesktop again. Two
/// tables keyed by the same path would mean the second call landing in a table
/// the third one does not read, which is a session that asked to see the
/// screen and is handed no stream.
#[derive(Debug, Default)]
pub struct Session {
    /// Which application the frontend says is asking, which is what a
    /// remembered source is filed under. From CreateSession rather than from
    /// each call: it is the frontend's statement about the application, and
    /// taking it once means the three calls cannot disagree.
    pub(super) app_id: String,
    /// What SelectSources asked for, which Start then acts on.
    pub(super) types: u32,
    /// Whether SelectSources was called on this session at all.
    ///
    /// Only a remote-desktop session has to ask: a plain screen share reaches
    /// Start through ScreenCast, where the answer is always yes. For a
    /// remote-desktop session it is the difference between a grant that also
    /// hands back a PipeWire node and one that only lets the application type,
    /// and `types` cannot say — a session that never selected sources and one
    /// that selected none both leave it at zero.
    pub(super) sources_selected: bool,
    /// Which interface created this session.
    ///
    /// A session begun on RemoteDesktop is finished on RemoteDesktop, and
    /// ScreenCast.Start on one is refused. The frontend does not do that, but
    /// the two interfaces sit on one bus name at one object path and the
    /// consequence of getting it wrong is a screen handed over by the call
    /// that was meant to ask about a keyboard.
    pub(super) remote: bool,
    /// What a remote-desktop session asked to be able to drive, as the
    /// interface's bitmask, and what the user actually allowed.
    ///
    /// Both, because they differ: the application asks for everything it might
    /// use and the person at the keyboard is the one who decides. Every
    /// Notify* call is checked against `granted`, which is zero until Start
    /// has been answered — so an application that skips the asking and starts
    /// typing is injecting nothing.
    pub(super) wanted_devices: u32,
    pub(super) granted_devices: u32,
    /// Whether this session has been handed a libei socket.
    ///
    /// Kept so that closing a session that has one says so, and closing one
    /// that has not — every screen share, and every remote-desktop session
    /// that stayed on the Notify calls — costs nothing. The compositor would
    /// ignore a revocation for a session it has no connection for, so this is
    /// not a correctness check; it is what keeps the common case from sending
    /// a message per closed share across the channel.
    pub(super) eis: bool,
    /// What this application shared last time, if the frontend recognised the
    /// token it presented.
    restore: Option<Remembered>,
    /// How long the application asked for the choice to be remembered: none,
    /// while it runs, or until revoked. Only the answer is this end's
    /// business — the frontend is what keeps the token — but it decides
    /// whether an answer is sent at all.
    persist: u32,
    /// The stream handed out, so closing the session stops it.
    pub(super) node: Option<u32>,
    /// Who asked — the portal frontend's connection, not the application's.
    ///
    /// Kept so a frontend that dies takes its sessions with it. Nothing else
    /// would: the frontend is what calls Close, and one that crashed will not,
    /// which leaves a compositor sharing a screen to nobody with no way to be
    /// told to stop.
    pub(super) owner: Option<String>,
}

impl Session {
    /// A conversation just begun, on whichever of the two interfaces began it.
    ///
    /// A constructor rather than a struct literal with `..Default::default()`
    /// because two modules build one of these and the rest of the fields —
    /// what was restored, how long to remember it, which node was handed out —
    /// are nobody's business outside this file. The three arguments are
    /// exactly what CreateSession knows.
    pub(super) fn new(app_id: &str, owner: Option<String>, remote: bool) -> Self {
        Self {
            app_id: app_id.to_owned(),
            owner,
            remote,
            ..Self::default()
        }
    }
}

/// What the object on the bus and the watcher both hold.
#[derive(Debug, Default)]
pub struct Frontend {
    /// One conversation each, by the handle the frontend named it with.
    ///
    /// One table for both interfaces. See [`Session`] for why they cannot be
    /// two.
    pub(super) sessions: HashMap<OwnedObjectPath, Session>,
    /// Who owns [`FRONTEND_NAME`] just now, as a unique name.
    ///
    /// Learned once when the watcher starts and kept up to date from
    /// NameOwnerChanged, rather than asked for on every call: the answer is a
    /// round trip on the connection the call arrived on, and this end is
    /// already holding up an application's dialogue while it asks.
    ///
    /// `None` is "nobody has said", and every call is refused while it is —
    /// there is no frontend to be talking to, so anything on this interface is
    /// something else.
    owner: Option<String>,
    /// What each application may ask for again without being asked, by the
    /// token this end minted for it.
    ///
    /// The blob the frontend stores used to describe the source itself, which
    /// made it a key: it is handed back to whoever presents it, and anybody on
    /// the session bus could write "all outputs" and hand it over. A token
    /// names a row here instead, and the row records which application the
    /// choice was made by — so restoring is a choice already made by *that*
    /// application rather than a sentence anybody can compose.
    ///
    /// In memory, so a compositor restart means the user is asked once more.
    /// That is the cost of the source not being in the token, and it is the
    /// right way round: the alternative is a token that means something on its
    /// own.
    remembered: Vec<(String, String, Remembered)>,
}

impl Frontend {
    /// Write a source down for an application, and say what to call it.
    fn remember(&mut self, app_id: &str, remembered: &Remembered) -> Option<String> {
        let token = mint()?;
        if self.remembered.len() >= REMEMBERED_LIMIT {
            self.remembered.remove(0);
        }
        self.remembered
            .push((token.clone(), app_id.to_owned(), remembered.clone()));
        Some(token)
    }

    /// What a token means, for the application it was minted for.
    ///
    /// Nothing for anybody else's token, which is what keeps one application's
    /// permission from being another's: the frontend files them per
    /// application, and this end does not take that on trust either.
    fn recall(&self, token: &str, app_id: &str) -> Option<Remembered> {
        self.remembered
            .iter()
            .find(|(minted, owner, _)| minted == token && owner == app_id)
            .map(|(_, _, remembered)| remembered.clone())
    }
}

/// The shared state, between the object on the bus and the watcher that
/// notices a frontend arriving or disappearing.
pub type Sessions = Arc<Mutex<Frontend>>;

/// Whether a call came from the portal frontend, and a line in the log if it
/// did not.
///
/// Written as a free function rather than a method because both interfaces
/// served on this bus name need it and both need exactly the same answer.
/// ScreenCast hands out a picture of the desk; RemoteDesktop hands out the
/// keyboard, so if anything the second is the one that must not be reachable
/// by a peer that merely knows the name — and the session bus is reachable by
/// every process in the session.
///
/// `what` names the interface in the log line, so a refused call can be told
/// from its neighbour when both are being tried.
pub fn called_by_frontend(
    sessions: &Sessions,
    what: &str,
    header: &zbus::message::Header<'_>,
) -> bool {
    let sender = header.sender().map(|name| name.as_str());
    let owner = sessions.lock().unwrap().owner.clone();
    match (owner.as_deref(), sender) {
        (Some(owner), Some(sender)) if owner == sender => true,
        _ => {
            tracing::warn!(
                "{what}: refusing a call from {sender:?} — the portal frontend is {owner:?}"
            );
            false
        }
    }
}

/// The object on the bus.
pub struct ScreenCast {
    sender: smithay::reexports::calloop::channel::Sender<Message>,
    sessions: Sessions,
}

impl ScreenCast {
    pub fn new(sender: smithay::reexports::calloop::channel::Sender<Message>) -> Self {
        Self {
            sender,
            sessions: Arc::new(Mutex::new(Frontend::default())),
        }
    }

    /// The shared state, for the watcher that follows the frontend.
    pub fn sessions(&self) -> Sessions {
        self.sessions.clone()
    }

    /// Whether a call came from the portal frontend, and a line in the log if
    /// it did not.
    ///
    /// Every method on this interface goes through it. The frontend is the only
    /// peer that has any business here — it is what asks the user's application
    /// what it wants and what carries the answer back — and a call from
    /// anywhere else is a process on the session bus asking this compositor to
    /// hand it a screen.
    fn called_by_frontend(&self, header: &zbus::message::Header<'_>) -> bool {
        called_by_frontend(&self.sessions, "screencast", header)
    }

    /// A way to tell the compositor to stop, for the same watcher.
    pub fn closer(&self) -> smithay::reexports::calloop::channel::Sender<Message> {
        self.sender.clone()
    }

    /// Ask the compositor, and wait for it.
    ///
    /// Awaited rather than blocked on: the answer is a person choosing what to
    /// share, which takes as long as it takes, and this call is running on the
    /// bus connection that also has to carry Close and the settings the rest of
    /// the desktop is asking for.
    ///
    /// Unbounded here, and bounded at the other end. The compositor is the side
    /// that knows whether a chooser is still on screen, and it answers a user
    /// who walked away by cancelling. Dropping the chooser drops this sender
    /// too, which ends the wait either way.
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

#[zbus::interface(name = "org.freedesktop.impl.portal.ScreenCast")]
impl ScreenCast {
    /// Both kinds. A monitor is what every portal offers; a window is the
    /// reason this one exists.
    #[zbus(property, name = "AvailableSourceTypes")]
    fn available_source_types(&self) -> u32 {
        super::SOURCE_MONITOR | super::SOURCE_WINDOW
    }

    /// The cursor is drawn into the frames, which is "embedded".
    ///
    /// Not "metadata": that promises a separate cursor position on every
    /// frame, and a client that asks for it and is sent pixels instead draws
    /// no pointer at all.
    #[zbus(property, name = "AvailableCursorModes")]
    fn available_cursor_modes(&self) -> u32 {
        // 1 hidden, 2 embedded, 4 metadata.
        2
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        4
    }

    /// The application is starting a conversation.
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
        tracing::debug!("screencast: create session {path} for {app_id:?}");
        self.sessions
            .lock()
            .unwrap()
            .sessions
            .insert(path.clone(), Session::new(app_id, owner, false));

        // The session object the frontend closes when the application is
        // done. Without it a share runs until the compositor exits: nothing
        // else tells this end that the browser tab was closed, and the
        // compositor keeps drawing frames for a stream nobody is reading.
        let session = SessionObject {
            path: path.clone(),
            sender: self.sender.clone(),
            sessions: self.sessions.clone(),
        };
        if let Err(e) = server.at(&path, session).await {
            tracing::warn!("could not publish a screencast session: {e}");
        }
        (RESPONSE_SUCCESS, HashMap::new())
    }

    /// What kind of thing the application wants to share.
    ///
    /// Remembered rather than acted on: nothing is picked until Start, which
    /// is where the user is asked.
    fn select_sources(
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
        let types = options
            .get("types")
            .and_then(|value| u32::try_from(value).ok())
            // A client that says nothing means a monitor, which is what the
            // interface has always defaulted to.
            .unwrap_or(super::SOURCE_MONITOR);

        // What the application shared last time. The application holds an
        // opaque token; the frontend is what turns it back into the blob this
        // end minted, and it only does so for a blob it issued to the same
        // application — and the blob is then looked up here against the
        // application it was minted for, so restoring without asking is a
        // continuation of a choice already made rather than a new one made on
        // whatever the data happened to say.
        let token = options.get("restore_data").and_then(decode);
        let persist = options
            .get("persist_mode")
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(PERSIST_NONE);

        let path = OwnedObjectPath::from(session_handle);
        let mut shared = self.sessions.lock().unwrap();
        // Which application this is, before the token is looked up: a
        // remembered source belongs to the application that agreed to it, and
        // that is what the session says rather than what the data says.
        let Some(app_id) = shared
            .sessions
            .get(&path)
            .map(|session| session.app_id.clone())
        else {
            tracing::warn!("screencast: select sources for a session that does not exist");
            return (RESPONSE_FAILED, HashMap::new());
        };
        let restore = token.and_then(|token| shared.recall(&token, &app_id));

        tracing::debug!(
            "screencast: select sources, types {types}, persist {persist}, \
             restoring {restore:?}"
        );
        let Some(session) = shared.sessions.get_mut(&path) else {
            return (RESPONSE_FAILED, HashMap::new());
        };
        session.types = types;
        session.restore = restore;
        session.persist = persist;
        // Said out loud, because a remote-desktop session reads it back. The
        // frontend calls this interface's SelectSources on a session that
        // RemoteDesktop created when the application wants to see the desk as
        // well as drive it, and RemoteDesktop.Start has no other way to tell
        // that apart from a session that only wants the keyboard.
        session.sources_selected = true;
        (RESPONSE_SUCCESS, HashMap::new())
    }

    /// A connection to PipeWire, for the application to read the stream
    /// through.
    ///
    /// Without this the application has a node number and no way to reach it:
    /// it builds its pw_core from this descriptor, so a portal that does not
    /// answer leaves the stream sitting at Paused with nothing negotiating —
    /// which is a share that hands back a node and produces nothing, and says
    /// so nowhere.
    ///
    /// A fresh connection to the daemon rather than the compositor's own: the
    /// application gets its own client, and closing it is its business.
    fn open_pipe_wire_remote(
        &self,
        _session_handle: ObjectPath<'_>,
        _options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<zvariant::OwnedFd> {
        if !self.called_by_frontend(&header) {
            return Err(zbus::fdo::Error::AccessDenied(
                "that is not the portal frontend".to_owned(),
            ));
        }
        tracing::debug!("screencast: the application asked for a pipewire connection");
        let socket = pipewire_socket()
            .ok_or_else(|| zbus::fdo::Error::Failed("no pipewire socket".to_owned()))?;
        let stream = std::os::unix::net::UnixStream::connect(&socket).map_err(|e| {
            zbus::fdo::Error::Failed(format!("connecting to {}: {e}", socket.display()))
        })?;
        Ok(zvariant::OwnedFd::from(std::os::fd::OwnedFd::from(stream)))
    }

    /// Pick a source and hand back the stream.
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
        let (app_id, types, restore, persist) = {
            let shared = self.sessions.lock().unwrap();
            match shared.sessions.get(&path) {
                // A session that RemoteDesktop created is finished on
                // RemoteDesktop, whose Start asks about the keyboard as well
                // as the screen. Answering it here would hand over a picture
                // of the desk from the one call in the conversation that was
                // never going to mention that anyone could type into it.
                Some(session) if session.remote => {
                    tracing::warn!(
                        "screencast: refusing to start {path} — it is a remote-desktop session"
                    );
                    return (RESPONSE_FAILED, HashMap::new());
                }
                Some(session) => (
                    session.app_id.clone(),
                    session.types,
                    session.restore.clone(),
                    session.persist,
                ),
                None => return (RESPONSE_FAILED, HashMap::new()),
            }
        };

        // One slot: there is one answer, and the compositor must be able to
        // hand it over without blocking on this end having got there first.
        let (sender, receiver) = async_channel::bounded(1);
        let started = match self
            .ask(
                Message::Start {
                    types,
                    restore,
                    reply: sender,
                },
                receiver,
            )
            .await
        {
            Ok(started) => started,
            Err(e) => {
                tracing::warn!("screencast: {e}");
                // Cancelled rather than failed: the usual reason is that there
                // was nothing to pick, or the user picked nothing, and an
                // application shows a failure as an error box.
                return (RESPONSE_CANCELLED, HashMap::new());
            }
        };

        if let Some(session) = self.sessions.lock().unwrap().sessions.get_mut(&path) {
            session.node = Some(started.node);
        }

        // One stream, described the way the interface wants it: the node to
        // connect to, and a dictionary the application reads for its size.
        let mut properties: HashMap<String, Value<'static>> = HashMap::new();
        properties.insert(
            "size".to_owned(),
            Value::from((started.width, started.height)),
        );
        properties.insert("source_type".to_owned(), Value::from(started.source_type));
        let streams = vec![(started.node, properties)];

        let mut results: HashMap<String, OwnedValue> = HashMap::new();
        match OwnedValue::try_from(Value::from(streams)) {
            Ok(value) => {
                results.insert("streams".to_owned(), value);
                tracing::debug!(
                    "screencast: answering with node {} at {}x{}",
                    started.node,
                    started.width,
                    started.height
                );
            }
            // The frontend has nothing to pass on without this, and an
            // application shown an empty result waits for a stream that will
            // never be named.
            Err(e) => tracing::warn!("screencast: could not describe the stream: {e}"),
        }

        // And what to ask for next time, if the application wanted that.
        //
        // Both keys or neither: the frontend stores the data against the mode,
        // and data with no mode is a token it mints and then throws away,
        // which is a restore that silently never happens.
        //
        // A token rather than a description of the source: what goes to the
        // frontend names a row in this end's table, and the row is what says
        // which source and which application. See [`Frontend::remembered`].
        if persist != PERSIST_NONE {
            let token = started
                .remembered
                .as_ref()
                .and_then(|remembered| self.sessions.lock().unwrap().remember(&app_id, remembered));
            match token.as_deref().map(encode) {
                Some(Ok(data)) => {
                    results.insert("restore_data".to_owned(), data);
                    results.insert("persist_mode".to_owned(), OwnedValue::from(persist));
                }
                // Nothing to restore from, either because writing it down
                // failed or because the compositor shared something it cannot
                // name again. Said out loud as a zero rather than by leaving
                // the key out: a Start that answers with no mode is read as
                // the mode that was asked for, and the frontend would keep a
                // permission for a token it can never fill in.
                other => {
                    if let Some(Err(e)) = other {
                        tracing::warn!("screencast: could not write down what was shared: {e}");
                    }
                    results.insert("persist_mode".to_owned(), OwnedValue::from(PERSIST_NONE));
                }
            }
        }
        (RESPONSE_SUCCESS, results)
    }
}

/// A name for a remembered source that means nothing on its own.
///
/// Sixteen bytes of the kernel's randomness, in hex. Unguessable because the
/// frontend is not the only thing that ever sees one — it goes to disk in the
/// permission store — and a token that could be guessed would be a screen share
/// anybody could ask for by writing one out.
///
/// `None` rather than something derived from the clock if the kernel will not
/// answer: a predictable token is worse than a share the user is asked about
/// again.
fn mint() -> Option<String> {
    use std::io::Read as _;

    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|e| tracing::warn!("screencast: could not mint a restore token: {e}"))
        .ok()?;
    Some(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Write a token down as the interface carries it: `(suv)`, which is who wrote
/// it, which shape it is in, and the thing itself.
///
/// A dictionary inside rather than a bare string, because a reader of the next
/// version has to be able to skip what it does not know.
fn encode(token: &str) -> zvariant::Result<OwnedValue> {
    let mut fields: HashMap<String, Value<'static>> = HashMap::new();
    fields.insert("token".to_owned(), Value::from(token.to_owned()));

    let data = Value::Value(Box::new(Value::from(zvariant::Dict::from(fields))));
    OwnedValue::try_from(Value::from((RESTORE_VENDOR, RESTORE_VERSION, data)))
}

/// Read one back, if it is one of ours.
///
/// Every way of not being one is the same answer — a different compositor
/// wrote it, a later version of this one did, the frontend handed back
/// something malformed. None of them are worth failing the call over: the user
/// is asked, which is what would have happened without a token at all. So is a
/// token this end no longer has a row for, which is every token from before the
/// compositor was restarted.
fn decode(value: &OwnedValue) -> Option<String> {
    let Value::Structure(structure) = &**value else {
        return None;
    };
    let [vendor, version, data] = structure.fields() else {
        return None;
    };
    let (Value::Str(vendor), Value::U32(version)) = (inside(vendor), inside(version)) else {
        return None;
    };
    if vendor.as_str() != RESTORE_VENDOR || *version != RESTORE_VERSION {
        tracing::debug!("screencast: ignoring restore data from {vendor} version {version}");
        return None;
    }

    let Value::Dict(dict) = inside(data) else {
        return None;
    };
    let field = |wanted: &str| -> Option<String> {
        dict.iter().find_map(|(key, value)| match inside(key) {
            Value::Str(key) if key.as_str() == wanted => match inside(value) {
                Value::Str(value) => Some(value.as_str().to_owned()),
                _ => None,
            },
            _ => None,
        })
    };

    field("token").filter(|token| !token.is_empty())
}

/// What is inside a variant, however many of them there are.
///
/// A dictionary of `a{sv}` is one wrapper deep when it comes off the wire and
/// none at all when it was built in this process, and a reader that assumes
/// either one is a reader that works in the tests and not on the bus.
fn inside<'v>(value: &'v Value<'v>) -> &'v Value<'v> {
    let mut value = value;
    while let Value::Value(inner) = value {
        value = inner;
    }
    value
}

/// The session object the frontend closes when the application is done.
///
/// One kind for both interfaces: a remote-desktop session publishes the same
/// object, and closing it does the same two things — takes the row out of the
/// table, which is what revokes a remote grant, and stops the stream if there
/// was one.
pub struct SessionObject {
    pub path: OwnedObjectPath,
    pub sender: smithay::reexports::calloop::channel::Sender<Message>,
    pub sessions: Sessions,
}

#[zbus::interface(name = "org.freedesktop.impl.portal.Session")]
impl SessionObject {
    /// The application has stopped sharing.
    async fn close(&self, #[zbus(object_server)] server: &zbus::ObjectServer) {
        tracing::debug!("portal: the frontend closed session {}", self.path);
        let closed = self.sessions.lock().unwrap().sessions.remove(&self.path);
        if let Some(node) = closed.as_ref().and_then(|session| session.node) {
            let _ = self.sender.send(Message::Close { node });
        }
        // And the libei socket, if this session was given one. Taking the row
        // out of the table is what revokes a Notify grant and it is not enough
        // here: the application is holding a socket, which goes on carrying
        // input until somebody closes it. See [`Message::RevokeEis`].
        if closed.as_ref().is_some_and(|session| session.eis) {
            let _ = self.sender.send(Message::RevokeEis {
                session: self.path.clone(),
            });
        }

        // And off the bus. Every share published one of these and none of them
        // ever went away, so a session that ran once left an object answering
        // for it for the rest of the session — which is a desktop that
        // accumulates dead screen shares for as long as it is up.
        if let Err(e) = server.remove::<SessionObject, _>(&self.path).await {
            tracing::warn!("could not take a closed session off the bus: {e}");
        }
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        2
    }
}

/// Where the PipeWire daemon is listening.
///
/// The same search the client library does: the name from the environment if
/// it is set, and pipewire-0 otherwise, inside the runtime directory.
fn pipewire_socket() -> Option<std::path::PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
    let name = std::env::var_os("PIPEWIRE_REMOTE").unwrap_or_else(|| "pipewire-0".into());
    Some(std::path::PathBuf::from(runtime).join(name))
}

/// Close every session a connection created, when that connection goes.
///
/// The frontend is what calls Close, so a frontend that crashed never will:
/// the compositor would go on compositing a screen into a stream nobody is
/// reading, holding its buffers, until the session ended. The bus says exactly
/// when a connection is gone, so that is what this waits for rather than
/// guessing from a stream that has been quiet for a while — a share that is
/// merely paused looks identical, and killing one the user is still holding
/// open is worse than leaking one they are not.
pub fn watch_frontend(
    connection: zbus::blocking::Connection,
    sessions: Sessions,
    sender: smithay::reexports::calloop::channel::Sender<Message>,
) {
    std::thread::Builder::new()
        .name("viewport-portal-watch".to_owned())
        .spawn(move || {
            let proxy = match zbus::blocking::fdo::DBusProxy::new(&connection) {
                Ok(proxy) => proxy,
                Err(e) => {
                    tracing::warn!("could not watch the portal frontend: {e}");
                    return;
                }
            };
            let changes = match proxy.receive_name_owner_changed() {
                Ok(changes) => changes,
                Err(e) => {
                    tracing::warn!("could not watch the portal frontend: {e}");
                    return;
                }
            };

            // Who has the name already. Subscribed to first, so a frontend that
            // arrives between the two is seen by the signal rather than missed
            // by both — the other way round is a window in which every call is
            // refused because nobody has said who the frontend is.
            match proxy.get_name_owner(FRONTEND_NAME.try_into().expect("a valid bus name")) {
                Ok(owner) => {
                    tracing::info!("the portal frontend is {owner}");
                    sessions.lock().unwrap().owner = Some(owner.to_string());
                }
                // Not up yet, which is ordinary: it is started on demand, and
                // the signal below is what says so when it is.
                Err(e) => tracing::debug!("no portal frontend yet: {e}"),
            }

            for change in changes {
                let Ok(args) = change.args() else { continue };
                // Who this compositor answers to, as that changes. Every call
                // on the screencast interface is checked against it: the
                // session bus is reachable by every process in the session, and
                // the calls on that interface hand out a screen.
                if args.name().as_str() == FRONTEND_NAME {
                    let owner = args.new_owner().as_ref().map(|owner| owner.to_string());
                    tracing::info!("the portal frontend is now {owner:?}");
                    sessions.lock().unwrap().owner = owner;
                }

                // A name that moved to a new owner is a service restarting,
                // not one that went away.
                if args.new_owner().is_some() {
                    continue;
                }
                let gone = args.name().to_string();

                let mut held = sessions.lock().unwrap();
                let closed: Vec<_> = held
                    .sessions
                    .iter()
                    .filter(|(_, session)| session.owner.as_deref() == Some(gone.as_str()))
                    .map(|(path, session)| (path.clone(), session.node, session.eis))
                    .collect();
                for (path, _, _) in &closed {
                    held.sessions.remove(path);
                }
                drop(held);

                for (path, node, eis) in closed {
                    tracing::info!("the portal frontend went away, closing session {path}");
                    if let Some(node) = node {
                        let _ = sender.send(Message::Close { node });
                    }
                    // A frontend that crashed is the case this whole watcher
                    // exists for, and it is the case where a libei socket
                    // matters most: nothing else will ever call Close, so a
                    // remote session left holding one would go on driving this
                    // machine with no portal left to stop it.
                    if eis {
                        let _ = sender.send(Message::RevokeEis {
                            session: path.clone(),
                        });
                    }
                    if let Err(e) = connection.object_server().remove::<SessionObject, _>(&path) {
                        tracing::warn!("could not take an abandoned session off the bus: {e}");
                    }
                }
            }
        })
        .map(|_| ())
        .unwrap_or_else(|e| tracing::warn!("could not start the portal watcher: {e}"));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Through the wire and back, which is the only round trip that matters.
    ///
    /// The frontend writes the data to disk and reads it back into a fresh
    /// process, so what `decode` is handed has been serialised and parsed
    /// again — and a variant that survives inside this process can come back
    /// wrapped in another one. A test that passed the value straight from
    /// `encode` to `decode` would prove nothing about the case that happens.
    fn round_trip(token: &str) -> Option<String> {
        let encoded = encode(token).expect("writing it down");
        let context = zvariant::serialized::Context::new_dbus(zvariant::Endian::Little, 0);
        let bytes = zvariant::to_bytes(context, &encoded).expect("serialising it");
        let (value, _) = bytes
            .deserialize::<zvariant::Value<'_>>()
            .expect("parsing it");
        decode(&OwnedValue::try_from(value).expect("owning it"))
    }

    /// A token survives the frontend. One that did not is a share OBS has to
    /// be told about by hand on every launch.
    #[test]
    fn a_token_survives_the_frontend() {
        assert_eq!(round_trip("abc123").as_deref(), Some("abc123"));
    }

    /// Every kind of share can be asked for again, and from the table rather
    /// than from the blob: what the frontend keeps says nothing about which
    /// screen, so a restore is what this end wrote down for that application.
    #[test]
    fn every_source_is_remembered_for_its_application() {
        let mut frontend = Frontend::default();
        for remembered in [
            Remembered::Output("DP-1".to_owned()),
            Remembered::Window {
                app_id: "org.mozilla.firefox".to_owned(),
                title: "A tab".to_owned(),
            },
            Remembered::AllOutputs,
            Remembered::FollowWindow,
            Remembered::FollowOutput,
        ] {
            let token = frontend
                .remember("org.mozilla.firefox", &remembered)
                .expect("minting a token");
            assert_eq!(
                frontend.recall(&token, "org.mozilla.firefox").as_ref(),
                Some(&remembered)
            );
        }
    }

    /// One application's token is not another's permission. The frontend files
    /// them per application and this end does not take that on trust: a token
    /// that worked for whoever presented it would be a screen share any
    /// application could inherit by handing over somebody else's blob.
    #[test]
    fn a_token_is_no_use_to_another_application() {
        let mut frontend = Frontend::default();
        let token = frontend
            .remember("org.mozilla.firefox", &Remembered::AllOutputs)
            .expect("minting a token");
        assert_eq!(frontend.recall(&token, "com.obsproject.Studio"), None);
    }

    /// A token nobody minted names nothing, which is the whole of the fix: the
    /// data used to describe the source itself, so any process on the session
    /// bus could compose "all outputs" and be handed the desk without a
    /// chooser.
    #[test]
    fn an_invented_token_names_nothing() {
        let frontend = Frontend::default();
        assert_eq!(frontend.recall("00000000", "com.obsproject.Studio"), None);
    }

    /// Two shares of the same thing are two tokens. A minted name that
    /// repeated would be one guessable by anybody who had ever seen one.
    #[test]
    fn no_two_tokens_are_the_same() {
        let mut frontend = Frontend::default();
        let first = frontend
            .remember("foot", &Remembered::AllOutputs)
            .expect("minting a token");
        let second = frontend
            .remember("foot", &Remembered::AllOutputs)
            .expect("minting a token");
        assert_ne!(first, second);
    }

    /// The table does not grow for the rest of the session: a share is set up
    /// and taken down all day, and the oldest row goes at the ceiling.
    #[test]
    fn the_table_has_a_ceiling() {
        let mut frontend = Frontend::default();
        let first = frontend
            .remember("foot", &Remembered::AllOutputs)
            .expect("minting a token");
        for _ in 0..REMEMBERED_LIMIT {
            frontend.remember("foot", &Remembered::AllOutputs);
        }
        assert_eq!(frontend.remembered.len(), REMEMBERED_LIMIT);
        assert_eq!(frontend.recall(&first, "foot"), None);
    }

    /// A token another compositor wrote is not read as one of ours. The
    /// frontend hands back whatever it stored for the application, and a
    /// session that started under a different desktop leaves data whose fields
    /// mean something else.
    #[test]
    fn another_compositors_token_is_ignored() {
        let mut fields: HashMap<String, Value<'static>> = HashMap::new();
        fields.insert("token".to_owned(), Value::from("abc123"));
        let data = Value::Value(Box::new(Value::from(zvariant::Dict::from(fields))));
        let foreign =
            OwnedValue::try_from(Value::from(("someone-else", 1u32, data))).expect("building it");
        assert_eq!(decode(&foreign), None);
    }

    /// So is one from a later version of this one, which is what makes the
    /// version number worth carrying: a field that changes meaning in version
    /// two must not be read by version one.
    #[test]
    fn a_later_version_is_ignored() {
        let mut fields: HashMap<String, Value<'static>> = HashMap::new();
        fields.insert("token".to_owned(), Value::from("abc123"));
        let data = Value::Value(Box::new(Value::from(zvariant::Dict::from(fields))));
        let later = OwnedValue::try_from(Value::from((RESTORE_VENDOR, RESTORE_VERSION + 1, data)))
            .expect("building it");
        assert_eq!(decode(&later), None);
    }

    /// And so is the version that described the source rather than naming a
    /// row, which is exactly the data anybody on the bus could compose.
    #[test]
    fn the_self_describing_version_is_ignored() {
        let mut fields: HashMap<String, Value<'static>> = HashMap::new();
        fields.insert("kind".to_owned(), Value::from("all-outputs"));
        let data = Value::Value(Box::new(Value::from(zvariant::Dict::from(fields))));
        let old =
            OwnedValue::try_from(Value::from((RESTORE_VENDOR, 1u32, data))).expect("building it");
        assert_eq!(decode(&old), None);
    }

    /// Anything else is nothing, rather than a panic on the bus thread. The
    /// data comes from a file the compositor does not own, and a malformed one
    /// must end in the chooser rather than in a dead portal.
    #[test]
    fn nonsense_is_not_a_restore() {
        for value in [
            OwnedValue::from(7u32),
            OwnedValue::try_from(Value::from("not a structure")).expect("building it"),
            // The shape is right and the dictionary says nothing.
            OwnedValue::try_from(Value::from((
                RESTORE_VENDOR,
                RESTORE_VERSION,
                Value::Value(Box::new(Value::from(zvariant::Dict::from(HashMap::<
                    String,
                    Value<'static>,
                >::new(
                ))))),
            )))
            .expect("building it"),
        ] {
            assert_eq!(decode(&value), None);
        }
    }
}
