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
const RESTORE_VERSION: u32 = 1;

/// The application does not want the choice remembered.
const PERSIST_NONE: u32 = 0;

/// One conversation with an application.
#[derive(Debug, Default)]
pub struct Session {
    /// What SelectSources asked for, which Start then acts on.
    types: u32,
    /// What this application shared last time, if the frontend recognised the
    /// token it presented.
    restore: Option<Remembered>,
    /// How long the application asked for the choice to be remembered: none,
    /// while it runs, or until revoked. Only the answer is this end's
    /// business — the frontend is what keeps the token — but it decides
    /// whether an answer is sent at all.
    persist: u32,
    /// The stream handed out, so closing the session stops it.
    node: Option<u32>,
    /// Who asked — the portal frontend's connection, not the application's.
    ///
    /// Kept so a frontend that dies takes its sessions with it. Nothing else
    /// would: the frontend is what calls Close, and one that crashed will not,
    /// which leaves a compositor sharing a screen to nobody with no way to be
    /// told to stop.
    owner: Option<String>,
}

/// The sessions, shared between the object on the bus and the watcher that
/// notices a frontend disappearing.
pub type Sessions = Arc<Mutex<HashMap<OwnedObjectPath, Session>>>;

/// The object on the bus.
pub struct ScreenCast {
    sender: smithay::reexports::calloop::channel::Sender<Message>,
    sessions: Sessions,
}

impl ScreenCast {
    pub fn new(sender: smithay::reexports::calloop::channel::Sender<Message>) -> Self {
        Self {
            sender,
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The sessions, for the watcher that closes them when a frontend goes.
    pub fn sessions(&self) -> Sessions {
        self.sessions.clone()
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
        _app_id: &str,
        _options: HashMap<String, OwnedValue>,
        #[zbus(object_server)] server: &zbus::ObjectServer,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        let path = OwnedObjectPath::from(session_handle);
        let owner = header.sender().map(|name| name.to_string());
        tracing::debug!("screencast: create session {path}");
        self.sessions.lock().unwrap().insert(
            path.clone(),
            Session {
                owner,
                ..Session::default()
            },
        );

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
    ) -> (u32, HashMap<String, OwnedValue>) {
        let types = options
            .get("types")
            .and_then(|value| u32::try_from(value).ok())
            // A client that says nothing means a monitor, which is what the
            // interface has always defaulted to.
            .unwrap_or(super::SOURCE_MONITOR);

        // What the application shared last time. The application holds an
        // opaque token; the frontend is what turns it back into this, and it
        // only does so for a token it issued to the same application, which is
        // what makes restoring without asking a continuation of a choice
        // already made rather than a new one made on the application's say-so.
        let restore = options.get("restore_data").and_then(decode);
        let persist = options
            .get("persist_mode")
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(PERSIST_NONE);

        tracing::debug!(
            "screencast: select sources, types {types}, persist {persist}, \
             restoring {restore:?}"
        );
        let mut sessions = self.sessions.lock().unwrap();
        let Some(session) = sessions.get_mut(&OwnedObjectPath::from(session_handle)) else {
            tracing::warn!("screencast: select sources for a session that does not exist");
            return (RESPONSE_FAILED, HashMap::new());
        };
        session.types = types;
        session.restore = restore;
        session.persist = persist;
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
    ) -> zbus::fdo::Result<zvariant::OwnedFd> {
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
    ) -> (u32, HashMap<String, OwnedValue>) {
        let path = OwnedObjectPath::from(session_handle);
        let (types, restore, persist) = {
            let sessions = self.sessions.lock().unwrap();
            match sessions.get(&path) {
                Some(session) => (session.types, session.restore.clone(), session.persist),
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

        if let Some(session) = self.sessions.lock().unwrap().get_mut(&path) {
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
        if persist != PERSIST_NONE {
            match started.remembered.as_ref().map(encode) {
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

/// Write a source down as the interface carries it: `(suv)`, which is who
/// wrote it, which shape it is in, and the thing itself.
///
/// A dictionary inside rather than a bare string, because the fields differ by
/// kind and a reader of the next version has to be able to skip what it does
/// not know.
fn encode(remembered: &Remembered) -> zvariant::Result<OwnedValue> {
    let mut fields: HashMap<String, Value<'static>> = HashMap::new();
    let kind = match remembered {
        Remembered::Output(name) => {
            fields.insert("output".to_owned(), Value::from(name.clone()));
            "output"
        }
        Remembered::Window { app_id, title } => {
            fields.insert("app_id".to_owned(), Value::from(app_id.clone()));
            fields.insert("title".to_owned(), Value::from(title.clone()));
            "window"
        }
        Remembered::AllOutputs => "all-outputs",
        Remembered::FollowWindow => "follow-window",
        Remembered::FollowOutput => "follow-output",
    };
    fields.insert("kind".to_owned(), Value::from(kind));

    let data = Value::Value(Box::new(Value::from(zvariant::Dict::from(fields))));
    OwnedValue::try_from(Value::from((RESTORE_VENDOR, RESTORE_VERSION, data)))
}

/// Read one back, if it is one of ours.
///
/// Every way of not being one is the same answer — a different compositor
/// wrote it, a later version of this one did, the frontend handed back
/// something malformed, the window kind names a field it did not send. None of
/// them are worth failing the call over: the user is asked, which is what
/// would have happened without a token at all.
fn decode(value: &OwnedValue) -> Option<Remembered> {
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

    match field("kind")?.as_str() {
        "output" => Some(Remembered::Output(field("output")?)),
        "window" => Some(Remembered::Window {
            app_id: field("app_id")?,
            title: field("title")?,
        }),
        "all-outputs" => Some(Remembered::AllOutputs),
        "follow-window" => Some(Remembered::FollowWindow),
        "follow-output" => Some(Remembered::FollowOutput),
        other => {
            tracing::debug!("screencast: restore data names a source kind, {other}, nobody offers");
            None
        }
    }
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
pub struct SessionObject {
    pub path: OwnedObjectPath,
    pub sender: smithay::reexports::calloop::channel::Sender<Message>,
    pub sessions: Sessions,
}

#[zbus::interface(name = "org.freedesktop.impl.portal.Session")]
impl SessionObject {
    /// The application has stopped sharing.
    async fn close(&self, #[zbus(object_server)] server: &zbus::ObjectServer) {
        tracing::debug!("screencast: the frontend closed a session");
        let node = self
            .sessions
            .lock()
            .unwrap()
            .remove(&self.path)
            .and_then(|session| session.node);
        if let Some(node) = node {
            let _ = self.sender.send(Message::Close { node });
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

            for change in changes {
                let Ok(args) = change.args() else { continue };
                // A name that moved to a new owner is a service restarting,
                // not one that went away.
                if args.new_owner().is_some() {
                    continue;
                }
                let gone = args.name().to_string();

                let mut held = sessions.lock().unwrap();
                let closed: Vec<_> = held
                    .iter()
                    .filter(|(_, session)| session.owner.as_deref() == Some(gone.as_str()))
                    .map(|(path, session)| (path.clone(), session.node))
                    .collect();
                for (path, _) in &closed {
                    held.remove(path);
                }
                drop(held);

                for (path, node) in closed {
                    tracing::info!("the portal frontend went away, closing session {path}");
                    if let Some(node) = node {
                        let _ = sender.send(Message::Close { node });
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
    fn round_trip(remembered: &Remembered) -> Option<Remembered> {
        let encoded = encode(remembered).expect("writing it down");
        let context = zvariant::serialized::Context::new_dbus(zvariant::Endian::Little, 0);
        let bytes = zvariant::to_bytes(context, &encoded).expect("serialising it");
        let (value, _) = bytes
            .deserialize::<zvariant::Value<'_>>()
            .expect("parsing it");
        decode(&OwnedValue::try_from(value).expect("owning it"))
    }

    /// Every kind of share can be asked for again. A kind that could not be
    /// written down is one OBS has to be told about by hand on every launch.
    #[test]
    fn every_source_survives_the_frontend() {
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
            assert_eq!(round_trip(&remembered).as_ref(), Some(&remembered));
        }
    }

    /// A token another compositor wrote is not read as one of ours. The
    /// frontend hands back whatever it stored for the application, and a
    /// session that started under a different desktop leaves data whose fields
    /// mean something else — restoring from it would share whatever this
    /// reader made of somebody else's dictionary.
    #[test]
    fn another_compositors_token_is_ignored() {
        let mut fields: HashMap<String, Value<'static>> = HashMap::new();
        fields.insert("kind".to_owned(), Value::from("output"));
        fields.insert("output".to_owned(), Value::from("DP-1"));
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
        fields.insert("kind".to_owned(), Value::from("output"));
        fields.insert("output".to_owned(), Value::from("DP-1"));
        let data = Value::Value(Box::new(Value::from(zvariant::Dict::from(fields))));
        let later = OwnedValue::try_from(Value::from((RESTORE_VENDOR, RESTORE_VERSION + 1, data)))
            .expect("building it");
        assert_eq!(decode(&later), None);
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

    /// A window remembered without its title is not restored to some other
    /// window of the same application by accident of the fields being there.
    /// Both are written and both are required back.
    #[test]
    fn a_window_needs_both_names() {
        let mut fields: HashMap<String, Value<'static>> = HashMap::new();
        fields.insert("kind".to_owned(), Value::from("window"));
        fields.insert("app_id".to_owned(), Value::from("foot"));
        let data = Value::Value(Box::new(Value::from(zvariant::Dict::from(fields))));
        let half = OwnedValue::try_from(Value::from((RESTORE_VENDOR, RESTORE_VERSION, data)))
            .expect("building it");
        assert_eq!(decode(&half), None);
    }
}
