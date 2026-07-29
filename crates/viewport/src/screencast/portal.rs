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
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

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
        reply: mpsc::Sender<Result<Started, String>>,
    },
    Close {
        node: u32,
    },
}

/// What a client needs to receive the stream.
#[derive(Debug, Clone, Copy)]
pub struct Started {
    pub node: u32,
    pub width: i32,
    pub height: i32,
    /// Which kind it turned out to be — a request for either is answered with
    /// whichever the user picked.
    pub source_type: u32,
}

/// The response codes the portal interface uses.
const RESPONSE_SUCCESS: u32 = 0;
const RESPONSE_CANCELLED: u32 = 1;
const RESPONSE_FAILED: u32 = 2;

/// One conversation with an application.
#[derive(Debug, Default)]
struct Session {
    /// What SelectSources asked for, which Start then acts on.
    types: u32,
    /// The stream handed out, so closing the session stops it.
    node: Option<u32>,
}

/// The object on the bus.
pub struct ScreenCast {
    sender: smithay::reexports::calloop::channel::Sender<Message>,
    sessions: Arc<Mutex<HashMap<OwnedObjectPath, Session>>>,
}

impl ScreenCast {
    pub fn new(sender: smithay::reexports::calloop::channel::Sender<Message>) -> Self {
        Self {
            sender,
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Ask the compositor, and wait for it.
    ///
    /// Bounded, because the frontend is waiting on this call and an
    /// application that gets no answer at all hangs its own dialogue. A
    /// compositor that is too busy to answer in a second is one the user is
    /// not currently sharing from.
    fn ask(&self, message: Message, reply: mpsc::Receiver<Result<Started, String>>) -> Result<Started, String> {
        self.sender
            .send(message)
            .map_err(|_| "the compositor is not listening".to_owned())?;
        reply
            .recv_timeout(std::time::Duration::from_secs(1))
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
    ) -> (u32, HashMap<String, OwnedValue>) {
        let path = OwnedObjectPath::from(session_handle);
        self.sessions
            .lock()
            .unwrap()
            .insert(path.clone(), Session::default());

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

        let mut sessions = self.sessions.lock().unwrap();
        let Some(session) = sessions.get_mut(&OwnedObjectPath::from(session_handle)) else {
            return (RESPONSE_FAILED, HashMap::new());
        };
        session.types = types;
        (RESPONSE_SUCCESS, HashMap::new())
    }

    /// Pick a source and hand back the stream.
    fn start(
        &self,
        _handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        _app_id: &str,
        _parent_window: &str,
        _options: HashMap<String, OwnedValue>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        let path = OwnedObjectPath::from(session_handle);
        let types = {
            let sessions = self.sessions.lock().unwrap();
            match sessions.get(&path) {
                Some(session) => session.types,
                None => return (RESPONSE_FAILED, HashMap::new()),
            }
        };

        let (sender, receiver) = mpsc::channel();
        let started = match self.ask(Message::Start { types, reply: sender }, receiver) {
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
        if let Ok(value) = OwnedValue::try_from(Value::from(streams)) {
            results.insert("streams".to_owned(), value);
        }
        (RESPONSE_SUCCESS, results)
    }
}

/// The session object the frontend closes when the application is done.
pub struct SessionObject {
    pub path: OwnedObjectPath,
    pub sender: smithay::reexports::calloop::channel::Sender<Message>,
    pub sessions: Arc<Mutex<HashMap<OwnedObjectPath, Session>>>,
}

#[zbus::interface(name = "org.freedesktop.impl.portal.Session")]
impl SessionObject {
    /// The application has stopped sharing.
    fn close(&self) {
        let node = self
            .sessions
            .lock()
            .unwrap()
            .remove(&self.path)
            .and_then(|session| session.node);
        if let Some(node) = node {
            let _ = self.sender.send(Message::Close { node });
        }
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        2
    }
}
