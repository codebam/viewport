// SPDX-License-Identifier: GPL-3.0-or-later
//
// Global shortcuts: keys an application hears without having focus.
//
// Push-to-talk in a chat program, start and stop in a recorder, next track in
// a player that is not playing on this desk. All of them want a chord that
// fires while somebody is typing in another window, and on X11 all of them got
// it by grabbing the key from the server. There is no such call on Wayland,
// deliberately — a client that can grab a chord can grab every chord — so the
// replacement is `org.freedesktop.portal.GlobalShortcuts`: the application
// describes the shortcuts it wants, the compositor asks the person at the
// machine, and what fires afterwards arrives as a signal rather than as a key.
//
// Which makes this compositor the only thing in the session that can answer
// it. The chord has to be resolved before the focused client sees the key, and
// this is what resolves every other chord already — the binding table in
// `binding.rs`, matched inside the one input path in `input.rs`.
//
// **A grant is remembered, and a remote-desktop grant is not.** That looks
// inconsistent beside `screencast::remote`, which refuses to write a grant
// down at all, so it is worth saying why they differ. A remembered
// remote-desktop grant is a process that can type anything into this machine
// on the strength of a blob in a file. A remembered shortcut is one chord
// reaching one application, only while that application is running and holding
// a session, and the alternative is asking the same question at every login
// for the same push-to-talk key — which trains somebody to press Enter on a
// dialogue they have stopped reading. The spec's own model is that shortcuts
// persist; this follows it, and the record says which application asked for
// which chord so it can be read and deleted.
//
// The bus side runs on the portal thread and the deciding happens on the
// compositor's, which is the shape every other portal here takes: a message
// over a channel, an answer over a one-shot reply channel, and the chooser in
// between.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

use crate::screencast::portal::{called_by_frontend, Sessions};

const RESPONSE_SUCCESS: u32 = 0;
const RESPONSE_CANCELLED: u32 = 1;
const RESPONSE_FAILED: u32 = 2;

const INTERFACE: &str = "org.freedesktop.impl.portal.GlobalShortcuts";
const OBJECT_PATH: &str = "/org/freedesktop/portal/desktop";

/// One shortcut an application is asking for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Requested {
    /// The application's own name for it. It comes back on every activation,
    /// so this is how the application knows which of its shortcuts fired.
    pub id: String,
    /// What it is for, in the application's words. The only thing in the
    /// dialogue that explains why the key is being asked for.
    pub description: String,
    /// The chord the application would like, as the portal spells it —
    /// `LOGO+SHIFT+s`. Empty where the application left the choice to the
    /// desktop, which this compositor cannot make for it; see
    /// [`Granted::from_request`].
    pub trigger: String,
}

/// One shortcut that has been granted, as the compositor holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Granted {
    pub id: String,
    pub description: String,
    /// The chord, spelled as a config file spells it.
    pub chord: String,
    /// What actually matches a key press. Parsed once here rather than on
    /// every keystroke.
    pub modifiers: crate::binding::Modifiers,
    pub keysym: u32,
}

impl Granted {
    /// A request turned into something that can match a key, or nothing.
    ///
    /// Two ways to get nothing, and both are the application's doing. A
    /// trigger this keymap cannot parse — a chord naming a modifier that does
    /// not exist, or a key xkb has never heard of — cannot be matched against
    /// a press, so granting it would be agreeing to something that can never
    /// happen. And an empty trigger is the portal's way of saying "you choose"
    /// — which needs a shortcut editor to choose *in*, and there is none here;
    /// a compositor that invented a chord instead would be handing out a key
    /// nobody asked for and nobody can see.
    pub fn from_request(request: &Requested) -> Option<Self> {
        let binding = crate::binding::parse_chord(request.trigger.trim())?;
        // A key, not a mouse button or the wheel. `parse_chord` accepts those
        // because a config file may bind them; a global shortcut is a
        // keyboard shortcut, and a button that fired one would swallow a click
        // somewhere across the desktop.
        if binding.keysym == 0 {
            return None;
        }
        Some(Self {
            id: request.id.clone(),
            description: request.description.clone(),
            chord: binding.chord(),
            modifiers: binding.modifiers,
            keysym: binding.keysym,
        })
    }

    /// What the frontend is told about this shortcut.
    ///
    /// `trigger_description` is what the application shows in its own
    /// settings, so it is the chord spelled the way the desktop spells it —
    /// the same string the dialogue showed the person who agreed to it.
    pub fn described(&self) -> HashMap<String, OwnedValue> {
        HashMap::from([
            ("description".to_owned(), owned(self.description.as_str())),
            ("trigger_description".to_owned(), owned(self.chord.as_str())),
        ])
    }
}

fn owned<'a, T: Into<Value<'a>>>(value: T) -> OwnedValue {
    value
        .into()
        .try_to_owned()
        .expect("a shortcut description is never a file descriptor")
}

/// One granted shortcut, as the compositor holds it while the session lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    /// The session it belongs to, which is what a signal names and what
    /// closing takes away.
    pub session: OwnedObjectPath,
    pub app_id: String,
    pub shortcut: Granted,
}

/// A shortcut that has just fired, named the way the signal names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fired {
    pub session: OwnedObjectPath,
    pub id: String,
}

/// What the bus thread asks the compositor to do.
#[derive(Debug)]
pub enum Message {
    /// An application wants these shortcuts. The answer is what it may have.
    Bind {
        app_id: String,
        session: OwnedObjectPath,
        shortcuts: Vec<Requested>,
        reply: async_channel::Sender<Result<Vec<Granted>, String>>,
    },
    /// What does this session already hold?
    List {
        session: OwnedObjectPath,
        reply: async_channel::Sender<Result<Vec<Granted>, String>>,
    },
    /// The session ended: the frontend closed it, or the application went
    /// away. Either way the chords stop firing.
    Close { session: OwnedObjectPath },
}

/// The connection the `Activated` and `Deactivated` signals go out on.
///
/// Set once the portal object is on the bus, and read from the compositor
/// thread when a chord fires. Shared rather than passed because the compositor
/// is not the side that built the connection — see `appearance::start`, where
/// every portal interface on this bus name is served from one.
///
/// The mutex guards the slot, nothing more. Anything reading the connection
/// clones it out and lets the lock go first: an emit is socket I/O, and this
/// runs on the event loop.
#[derive(Clone, Default)]
pub struct Signals(Arc<Mutex<Option<zbus::blocking::Connection>>>);

impl Signals {
    pub fn set_connection(&self, connection: zbus::blocking::Connection) {
        *self.0.lock().unwrap() = Some(connection);
    }

    /// Tell the frontend a shortcut fired, or stopped firing.
    ///
    /// The timestamp is the event's own, in microseconds, because that is what
    /// the interface asks for: an application deciding whether a
    /// push-to-talk key is still held wants when the key moved, not when a
    /// D-Bus message happened to be written.
    pub fn emit(&self, activated: bool, session: &OwnedObjectPath, id: &str, timestamp: u64) {
        // Clone the connection out and drop the lock before writing to the
        // socket. This runs on the event loop, for the press *and* the release
        // of every granted chord; an emit that held the mutex across its
        // blocking write would let a slow or wedged session bus stall input
        // and rendering together, and a wedged bus would hold the lock away
        // from `set_connection` besides. The lock is worth microseconds, not
        // round trips.
        let connection = self.0.lock().unwrap().clone();
        let Some(connection) = connection else {
            return;
        };
        let member = if activated {
            "Activated"
        } else {
            "Deactivated"
        };
        let options: HashMap<String, OwnedValue> = HashMap::new();
        if let Err(e) = connection.emit_signal(
            None::<&str>,
            OBJECT_PATH,
            INTERFACE,
            member,
            &(session.as_ref(), id, timestamp, options),
        ) {
            tracing::warn!("shortcuts: could not say {member} for {id}: {e}");
        }
    }
}

/// Which application asked for what, kept between sessions.
///
/// Keyed by app id, because that is what the person agreed about: "Discord may
/// hear Mod4+grave" outlives the session handle it was granted through, and
/// the next session that application opens is the same application. An app id
/// the frontend could not determine is never written down — an empty key would
/// pool every unidentifiable application into one grant, and the first of them
/// to ask would be agreeing on behalf of the rest.
#[derive(Debug, Default)]
pub struct Store {
    /// app id → shortcut id → the chord that was agreed to.
    granted: HashMap<String, HashMap<String, String>>,
    path: Option<PathBuf>,
}

impl Store {
    /// Read what was agreed to before, from the state directory.
    ///
    /// A file that is missing, unreadable or malformed is an empty store: the
    /// consequence is one dialogue that could have been skipped, and the
    /// alternative — refusing to start, or granting on a guess — is worse in
    /// both directions.
    pub fn load(path: Option<PathBuf>) -> Self {
        let granted = path
            .as_ref()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        Self { granted, path }
    }

    /// Where the record lives, beside the saved layout.
    pub fn default_path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
            })?;
        Some(base.join("viewport/shortcuts.json"))
    }

    /// Whether this application already agreed to exactly these chords.
    ///
    /// Exactly, rather than "at least": an application that changes a
    /// shortcut's chord between versions is asking a new question, and one
    /// that adds a shortcut is asking about the new one. Anything not covered
    /// sends the whole request back to the dialogue, so what is on screen is
    /// always the full list the application will end up holding.
    pub fn covers(&self, app_id: &str, shortcuts: &[Granted]) -> bool {
        if app_id.is_empty() || shortcuts.is_empty() {
            return false;
        }
        let Some(known) = self.granted.get(app_id) else {
            return false;
        };
        shortcuts
            .iter()
            .all(|shortcut| known.get(&shortcut.id) == Some(&shortcut.chord))
    }

    /// Write down what was just agreed to.
    pub fn remember(&mut self, app_id: &str, shortcuts: &[Granted]) {
        if app_id.is_empty() {
            return;
        }
        let known = self.granted.entry(app_id.to_owned()).or_default();
        for shortcut in shortcuts {
            known.insert(shortcut.id.clone(), shortcut.chord.clone());
        }
        self.save();
    }

    fn save(&self) {
        let Some(path) = self.path.as_ref() else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match serde_json::to_string_pretty(&self.granted) {
            Ok(text) => {
                if let Err(e) = std::fs::write(path, text) {
                    tracing::warn!("shortcuts: could not write {}: {e}", path.display());
                }
            }
            Err(e) => tracing::warn!("shortcuts: could not describe what was granted: {e}"),
        }
    }
}

/// "one shortcut" or "three shortcuts", for a log line.
///
/// Small, and here rather than at each call site because every line that says
/// how many shortcuts something asked for reads better with the noun attached
/// and worse with a stray plural.
pub fn count(n: usize) -> String {
    if n == 1 {
        "1 shortcut".to_owned()
    } else {
        format!("{n} shortcuts")
    }
}

/// The object on the bus.
pub struct GlobalShortcuts {
    sender: smithay::reexports::calloop::channel::Sender<Message>,
    /// Only to answer the one question this interface shares with the others:
    /// whether a call came from the portal frontend. The session table itself
    /// is the compositor's, because the compositor is what a chord reaches.
    sessions: Sessions,
    /// Which application each session belongs to, as the frontend named it.
    /// Kept here because `BindShortcuts` does not carry the app id and
    /// `CreateSession` does.
    apps: Arc<Mutex<HashMap<OwnedObjectPath, String>>>,
}

impl GlobalShortcuts {
    pub fn new(
        sender: smithay::reexports::calloop::channel::Sender<Message>,
        sessions: Sessions,
    ) -> Self {
        Self {
            sender,
            sessions,
            apps: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn called_by_frontend(&self, header: &zbus::message::Header<'_>) -> bool {
        called_by_frontend(&self.sessions, "global shortcuts", header)
    }

    /// Ask the compositor, and wait for it.
    async fn ask(
        &self,
        message: Message,
        reply: async_channel::Receiver<Result<Vec<Granted>, String>>,
    ) -> Result<Vec<Granted>, String> {
        self.sender
            .send(message)
            .map_err(|_| "the compositor is not listening".to_owned())?;
        reply
            .recv()
            .await
            .map_err(|_| "the compositor did not answer".to_owned())?
    }
}

/// What a granted list looks like on the wire: `a(sa{sv})`, the shortcut's own
/// id beside what it is and what fires it.
type Described = Vec<(String, HashMap<String, OwnedValue>)>;

fn describe(shortcuts: &[Granted]) -> Described {
    shortcuts
        .iter()
        .map(|shortcut| (shortcut.id.clone(), shortcut.described()))
        .collect()
}

#[zbus::interface(name = "org.freedesktop.impl.portal.GlobalShortcuts")]
impl GlobalShortcuts {
    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        1
    }

    /// The application is starting a conversation.
    ///
    /// Nothing is granted here and nothing is asked: a session with no
    /// shortcuts bound to it is an application that has said hello, which is
    /// not a question anybody should be woken up for.
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
        tracing::debug!("shortcuts: session {path} for {app_id:?}");
        self.apps
            .lock()
            .unwrap()
            .insert(path.clone(), app_id.to_owned());

        let session = SessionObject {
            path: path.clone(),
            sender: self.sender.clone(),
            apps: self.apps.clone(),
        };
        if let Err(e) = server.at(&path, session).await {
            tracing::warn!("shortcuts: could not publish session {path}: {e}");
            self.apps.lock().unwrap().remove(&path);
            return (RESPONSE_FAILED, HashMap::new());
        }
        (RESPONSE_SUCCESS, HashMap::new())
    }

    /// The application is asking for these keys.
    ///
    /// The answer is the list it may actually have, which is what the
    /// interface requires: an application is told what it got rather than
    /// left to assume it got what it asked for.
    async fn bind_shortcuts(
        &self,
        _handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        shortcuts: Vec<(String, HashMap<String, OwnedValue>)>,
        _parent_window: &str,
        _options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        if !self.called_by_frontend(&header) {
            return (RESPONSE_FAILED, HashMap::new());
        }
        let session = OwnedObjectPath::from(session_handle);
        let app_id = self
            .apps
            .lock()
            .unwrap()
            .get(&session)
            .cloned()
            .unwrap_or_default();

        let requested: Vec<Requested> = shortcuts
            .iter()
            .map(|(id, options)| Requested {
                id: id.clone(),
                description: string(options, "description"),
                trigger: string(options, "preferred_trigger"),
            })
            .collect();

        let (reply, answer) = async_channel::bounded(1);
        let granted = self
            .ask(
                Message::Bind {
                    app_id,
                    session: session.clone(),
                    shortcuts: requested,
                    reply,
                },
                answer,
            )
            .await;

        match granted {
            Ok(granted) if granted.is_empty() => {
                // Nothing survived, which is a refusal rather than an empty
                // success: an application told it succeeded with no shortcuts
                // waits for keys that will never come.
                (RESPONSE_CANCELLED, HashMap::new())
            }
            Ok(granted) => {
                let results =
                    HashMap::from([("shortcuts".to_owned(), owned_shortcuts(describe(&granted)))]);
                (RESPONSE_SUCCESS, results)
            }
            Err(e) => {
                tracing::info!("shortcuts: refused — {e}");
                (RESPONSE_CANCELLED, HashMap::new())
            }
        }
    }

    /// What this session already holds.
    async fn list_shortcuts(
        &self,
        _handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        if !self.called_by_frontend(&header) {
            return (RESPONSE_FAILED, HashMap::new());
        }
        let session = OwnedObjectPath::from(session_handle);
        let (reply, answer) = async_channel::bounded(1);
        match self.ask(Message::List { session, reply }, answer).await {
            Ok(granted) => (
                RESPONSE_SUCCESS,
                HashMap::from([("shortcuts".to_owned(), owned_shortcuts(describe(&granted)))]),
            ),
            Err(e) => {
                tracing::debug!("shortcuts: nothing to list — {e}");
                (RESPONSE_FAILED, HashMap::new())
            }
        }
    }

    /// Declared for the introspection, and emitted from [`Signals`] where the
    /// key is actually pressed — the compositor thread knows, and this object
    /// is on the bus thread.
    #[zbus(signal)]
    async fn activated(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        session_handle: ObjectPath<'_>,
        shortcut_id: &str,
        timestamp: u64,
        options: HashMap<String, Value<'_>>,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn deactivated(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        session_handle: ObjectPath<'_>,
        shortcut_id: &str,
        timestamp: u64,
        options: HashMap<String, Value<'_>>,
    ) -> zbus::Result<()>;
}

fn owned_shortcuts(described: Described) -> OwnedValue {
    Value::from(described)
        .try_to_owned()
        .expect("a shortcut list is never a file descriptor")
}

fn string(options: &HashMap<String, OwnedValue>, key: &str) -> String {
    options
        .get(key)
        .and_then(|value| <&str>::try_from(value).ok())
        .unwrap_or_default()
        .to_owned()
}

/// The session object the frontend closes when the application is done.
struct SessionObject {
    path: OwnedObjectPath,
    sender: smithay::reexports::calloop::channel::Sender<Message>,
    apps: Arc<Mutex<HashMap<OwnedObjectPath, String>>>,
}

#[zbus::interface(name = "org.freedesktop.impl.portal.Session")]
impl SessionObject {
    async fn close(&self, #[zbus(object_server)] server: &zbus::ObjectServer) {
        tracing::debug!("shortcuts: the frontend closed session {}", self.path);
        self.apps.lock().unwrap().remove(&self.path);
        let _ = self.sender.send(Message::Close {
            session: self.path.clone(),
        });
        if let Err(e) = server.remove::<SessionObject, _>(&self.path).await {
            tracing::warn!(
                "shortcuts: could not take session {} off the bus: {e}",
                self.path
            );
        }
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        2
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn requested(id: &str, trigger: &str) -> Requested {
        Requested {
            id: id.to_owned(),
            description: "talk".to_owned(),
            trigger: trigger.to_owned(),
        }
    }

    /// The portal spells a chord in capitals and a config file spells it in
    /// the desktop's own words. Both have to end at the same key, and what is
    /// shown to the person answering is the second one — they are comparing it
    /// against a keyboard, not against a specification.
    #[test]
    fn a_portal_trigger_becomes_a_chord_this_desktop_spells() {
        let granted = Granted::from_request(&requested("talk", "LOGO+SHIFT+s"))
            .expect("a chord this keymap has");
        assert_eq!(granted.chord, "Mod4+Shift+s");
        assert!(granted.modifiers.logo && granted.modifiers.shift);
        assert_ne!(granted.keysym, 0);

        // SUPER and CTRL are the other spellings in the wild.
        let ctrl = Granted::from_request(&requested("stop", "CTRL+ALT+j")).expect("a chord");
        assert_eq!(ctrl.chord, "Ctrl+Alt+j");
    }

    /// A trigger that cannot be matched must not be granted. Agreeing to a
    /// chord that can never fire is agreeing to nothing while telling the
    /// application it has something.
    #[test]
    fn a_trigger_this_keymap_cannot_read_is_not_granted() {
        assert!(Granted::from_request(&requested("talk", "HYPER+q")).is_none());
        assert!(Granted::from_request(&requested("talk", "Mod4+NotAKey")).is_none());
        // "You choose" — which needs a shortcut editor to choose in.
        assert!(Granted::from_request(&requested("talk", "")).is_none());
    }

    /// A button is not a keyboard shortcut. `parse_chord` takes them because a
    /// config file may bind one, and a global shortcut that swallowed a click
    /// would take it from whatever window it landed on.
    #[test]
    fn a_mouse_button_is_not_a_global_shortcut() {
        assert!(Granted::from_request(&requested("talk", "Mod4+Mouse4")).is_none());
        assert!(Granted::from_request(&requested("talk", "Mod4+WheelUp")).is_none());
    }

    fn granted(id: &str, chord: &str) -> Granted {
        Granted {
            id: id.to_owned(),
            description: String::new(),
            chord: chord.to_owned(),
            modifiers: crate::binding::Modifiers::default(),
            keysym: 1,
        }
    }

    /// What was agreed to once is not asked about again — that is the whole
    /// point of writing it down, and the difference from a remote-desktop
    /// grant, which is never remembered.
    #[test]
    fn an_agreed_chord_is_not_asked_about_twice() {
        let mut store = Store::default();
        let shortcuts = [granted("talk", "Mod4+grave")];
        assert!(!store.covers("discord", &shortcuts));
        store.remember("discord", &shortcuts);
        assert!(store.covers("discord", &shortcuts));
        // And it is that application's agreement, not everybody's.
        assert!(!store.covers("obs", &shortcuts));
    }

    /// A changed chord, or an added shortcut, is a new question. Otherwise an
    /// application could move a granted shortcut onto a different key and keep
    /// the grant that was given for the old one.
    #[test]
    fn a_different_chord_is_a_new_question() {
        let mut store = Store::default();
        store.remember("discord", &[granted("talk", "Mod4+grave")]);
        assert!(!store.covers("discord", &[granted("talk", "Mod4+F1")]));
        assert!(!store.covers(
            "discord",
            &[granted("talk", "Mod4+grave"), granted("mute", "Mod4+m")]
        ));
    }

    /// An application the frontend could not name is never written down: one
    /// empty key would pool every unidentifiable application into a single
    /// grant, and the first to ask would be agreeing for all of them.
    #[test]
    fn an_unnamed_application_is_never_remembered() {
        let mut store = Store::default();
        let shortcuts = [granted("talk", "Mod4+grave")];
        store.remember("", &shortcuts);
        assert!(!store.covers("", &shortcuts));
    }
}
