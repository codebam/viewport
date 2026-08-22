// SPDX-License-Identifier: GPL-3.0-or-later
//
// Notifications. Ports src/notification.c.
//
// Nothing on a Linux desktop sends a notification to the compositor. They go
// over D-Bus, to whichever program has claimed org.freedesktop.Notifications —
// usually mako or dunst, which then draw their own windows. That works, but it
// means notification styling is a second configuration language in a second
// program, and the notifications themselves are ordinary layer-shell surfaces
// the shell knows nothing about.
//
// Here the compositor claims the name itself and forwards each notification to
// the shell, which draws it as part of the desktop. The styling is the
// stylesheet already open in the editor, and a notification can sit inside the
// shell's own idea of the screen rather than floating over it as a separate
// client.
//
// The D-Bus interface is small but exacting: identifiers must be reusable so a
// program can replace its own previous notification rather than stacking
// duplicates, actions must be answered by name, and closing must say why.
//
// It runs on a thread of its own. zbus wants an async runtime and this
// compositor's loop is GLib with calloop nested inside it; rather than make
// those three agree, the service owns a thread and talks to the compositor
// through a channel, which is the only shared state.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use viewport_ipc::event::{Notification, NotificationAction};

/// What the D-Bus thread sends the compositor.
#[derive(Debug)]
pub enum Message {
    Add(Box<Notification>),
    /// Closed by the sender, rather than by the user.
    Close(u32),
}

/// Why a notification ended, as `NotificationClosed` reports it.
///
/// The numbers are the specification's and a client may act on them: an
/// application that sees `Dismissed` knows the user saw it, and one that sees
/// `Expired` does not.
#[derive(Debug, Clone, Copy)]
pub enum CloseReason {
    Expired = 1,
    Dismissed = 2,
    ByRequest = 3,
}

/// The half of the service the compositor keeps.
pub struct Notifications {
    /// Signals back to D-Bus: a notification the user acted on or dismissed.
    outbound: Option<zbus::blocking::Connection>,
    /// The last id handed out, shared with the D-Bus thread because both ends
    /// allocate them.
    next: Arc<AtomicU32>,
    /// Which connection owns which id, by the sender's unique bus name.
    ///
    /// An id is a handle: whoever holds one can replace the notification,
    /// close it, and is the one its `ActionInvoked` and `NotificationClosed`
    /// go to. Recorded here when an id is given out, so that a stranger's
    /// `replaces_id` can be refused and a signal can be addressed to its
    /// owner rather than shouted at the whole session. Shared with the D-Bus
    /// thread, which is the only one that learns a sender's name.
    owners: Arc<Mutex<HashMap<u32, String>>>,
    /// What a notification that names no sound of its own plays.
    ///
    /// Behind a lock and shared with the D-Bus thread because the
    /// configuration is reloadable: `apply_config` writes it here and the
    /// server thread reads it on the next notification, rather than the
    /// setting being fixed at the moment the bus name was claimed.
    default_sound: Arc<Mutex<Option<crate::sound::Sound>>>,
}

impl Default for Notifications {
    fn default() -> Self {
        Self {
            outbound: None,
            // Zero means "allocate one" in the protocol, so ids start at one.
            next: Arc::new(AtomicU32::new(1)),
            owners: Arc::new(Mutex::new(HashMap::new())),
            default_sound: Arc::new(Mutex::new(None)),
        }
    }
}

impl Notifications {
    /// Claim the bus name and start serving.
    ///
    /// A failure here is not fatal: a session with no D-Bus, or one where mako
    /// already holds the name, still has a working compositor — it just has no
    /// notifications, which is what it had a moment ago anyway.
    pub fn start(
        &mut self,
        sender: smithay::reexports::calloop::channel::Sender<Message>,
    ) -> anyhow::Result<()> {
        let next = self.next.clone();
        let owners = self.owners.clone();
        let server = Server {
            sender,
            next: next.clone(),
            owners,
            default_sound: self.default_sound.clone(),
            // A session with no sound server gets no sounds and every other
            // part of a notification unchanged; see `sound::Player::new`.
            player: crate::sound::Player::new(),
        };

        let connection = zbus::blocking::connection::Builder::session()?
            .serve_at("/org/freedesktop/Notifications", server)?
            .build()?;

        // The name, asked for with flags of our own rather than the
        // builder's `.name()`, which uses zbus's defaults. See
        // `crate::dbus::name_flags`: the compositor queues for this name and
        // never takes it, so a notification daemon somebody starts wins and
        // the compositor gets it back when that daemon exits.
        let reply = connection
            .request_name_with_flags("org.freedesktop.Notifications", crate::dbus::name_flags());
        crate::dbus::log_name_reply("org.freedesktop.Notifications", reply);

        self.outbound = Some(connection);
        Ok(())
    }

    /// What to play for a notification that names no sound of its own.
    ///
    /// Called on every configuration load, including reloads, so this is also
    /// how a sound is taken away again — `None` silences the default without
    /// touching what a sender asks for explicitly.
    pub fn set_default_sound(&self, sound: Option<crate::sound::Sound>) {
        if let Ok(mut default) = self.default_sound.lock() {
            *default = sound;
        }
    }

    /// Tell the sender its notification was acted on.
    ///
    /// To the sender, and to it alone: an action is a private answer between
    /// the desktop and the application that offered the button, and a signal
    /// broadcast to every connection would let any program on the bus learn
    /// which buttons somebody presses.
    pub fn invoke_action(&self, id: u32, action: &str) {
        self.emit_to_owner(id, "ActionInvoked", &(id, action));
    }

    /// Tell the sender its notification is gone, and why — then forget whose
    /// it was. A closed id is finished with; keeping the owner would only
    /// grow this map for the life of the session.
    pub fn closed(&self, id: u32, reason: CloseReason) {
        self.emit_to_owner(id, "NotificationClosed", &(id, reason as u32));
        if let Ok(mut owners) = self.owners.lock() {
            owners.remove(&id);
        }
    }

    /// Emit one of the interface's signals, addressed to the notification's
    /// owner when this compositor knows who that is.
    ///
    /// An owner this process does not know — one from before a restart, say —
    /// falls back to broadcasting, which is what every version of this did
    /// and what keeps the ordinary case working across a reload.
    fn emit_to_owner<T: serde::Serialize + zvariant::DynamicType>(
        &self,
        id: u32,
        signal: &str,
        body: &T,
    ) {
        let destination = self
            .owners
            .lock()
            .ok()
            .and_then(|owners| owners.get(&id).cloned());
        self.emit(destination.as_deref(), signal, body);
    }

    fn emit<T: serde::Serialize + zvariant::DynamicType>(
        &self,
        destination: Option<&str>,
        signal: &str,
        body: &T,
    ) {
        let Some(connection) = self.outbound.as_ref() else {
            return;
        };
        if let Err(e) = connection.emit_signal(
            destination,
            "/org/freedesktop/Notifications",
            "org.freedesktop.Notifications",
            signal,
            body,
        ) {
            tracing::warn!("could not emit {signal}: {e}");
        }
    }
}

/// The object on the bus.
struct Server {
    sender: smithay::reexports::calloop::channel::Sender<Message>,
    next: Arc<AtomicU32>,
    /// Which connection owns which id; see `Notifications::owners`.
    owners: Arc<Mutex<HashMap<u32, String>>>,
    default_sound: Arc<Mutex<Option<crate::sound::Sound>>>,
    /// Absent when there is no sound server to play through.
    player: Option<crate::sound::Player>,
}

/// The id a notification gets, given what the sender asked to replace.
///
/// `replaces_id` is honoured only when the sender asking is the one the id
/// was given to. The specification scopes replacement to the sender, and for
/// good reason: an id is a handle, so honouring a stranger's guess would let
/// one application overwrite another's notification on screen, close it, and
/// receive the actions meant for it. mako tracks the same pair. Anything
/// else — a zero, an id nobody holds, somebody else's — is a fresh
/// allocation, which is the forgiving answer: the sender's notification
/// appears either way, and the owner of the named id keeps theirs.
///
/// `sender` absent means there is nobody to check against — the bus did not
/// say, which does not happen for a real call — and the id is then taken at
/// its word, as every version of this did before there was an owner to check.
fn resolve_id(
    next: &AtomicU32,
    owners: &HashMap<u32, String>,
    replaces_id: u32,
    sender: Option<&str>,
) -> u32 {
    let owned = match sender {
        Some(sender) => owners
            .get(&replaces_id)
            .is_some_and(|owner| owner == sender),
        None => replaces_id != 0,
    };
    if replaces_id != 0 && owned {
        // And the counter is moved past it. Both ends allocate ids, and a
        // replacement id honoured but not accounted for was handed out
        // again later as if it were fresh: two live notifications with one
        // id, and dismissing either closed the other.
        next.fetch_max(replaces_id.saturating_add(1), Ordering::Relaxed);
        return replaces_id;
    }
    if replaces_id != 0 {
        tracing::debug!(
            "notification replaces_id {replaces_id} is not this sender's; allocating fresh"
        );
    }
    next.fetch_add(1, Ordering::Relaxed)
}

/// Whether this caller may close this id.
///
/// An id whose owner is known is closed only by its owner — closing is the
/// other half of the handle `replaces_id` is, and a program that could close
/// anything could take every notification on the desktop down. An id nobody
/// claims stays closable by anyone, which is what it always was: refusing on
/// a guess is how a legitimate close stops working.
fn may_close(owners: &HashMap<u32, String>, id: u32, sender: Option<&str>) -> bool {
    match (owners.get(&id), sender) {
        (Some(owner), Some(sender)) => owner == sender,
        _ => true,
    }
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl Server {
    /// The one method that matters. Returns the id the sender should use to
    /// replace or close this notification later.
    ///
    /// The id is only returned for a notification that was actually handed
    /// over. A sender told `Ok` when the compositor side had gone would spend
    /// the rest of its life waiting for an action or a close that can never
    /// come; an error reply is how it learns to try again or give up.
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: String,
        replaces_id: u32,
        app_icon: String,
        summary: String,
        body: String,
        actions: Vec<String>,
        hints: HashMap<String, zvariant::OwnedValue>,
        expire_timeout: i32,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> Result<u32, zbus::fdo::Error> {
        // The unique name of whoever is asking, which is what replacement and
        // closing are checked against. Well-known names would be spoofable —
        // a program could ask for another's name and inherit its
        // notifications — but a unique name is handed out by the bus itself.
        let sender = header.sender().map(|name| name.to_string());

        let id = match self.owners.lock() {
            Ok(mut owners) => {
                let id = resolve_id(&self.next, &owners, replaces_id, sender.as_deref());
                // An id with no known owner stays unowned rather than owned
                // by an empty string: an unknown owner is one anybody may
                // close, and that is what it always was.
                if let Some(sender) = sender {
                    owners.insert(id, sender);
                }
                id
            }
            Err(_) => resolve_id(&self.next, &HashMap::new(), replaces_id, None),
        };

        // Before the notification goes to the shell, not after: the shell is a
        // web page being drawn and this is a queueing call that does not wait
        // for playback, so putting it first is the sound and the window
        // arriving together rather than the sound trailing the paint.
        //
        // A replacement sounds too, as it does in mako and dunst. The hint for
        // a sender that does not want that is `suppress-sound`, which is the
        // one it already has to set for its own progress bar not to be a
        // hundred beeps in every other notification daemon.
        if let Some(player) = self.player.as_ref() {
            let default = self.default_sound.lock().ok().and_then(|d| d.clone());
            if let Some(sound) = sound(&hints, default) {
                player.play(&sound);
            }
        }

        let notification = Notification {
            id,
            app_name,
            icon: app_icon,
            summary,
            body,
            urgency: urgency(&hints),
            timeout: expire_timeout,
            actions: parse_actions(&actions),
            at: now(),
        };
        if let Err(e) = self.sender.send(Message::Add(Box::new(notification))) {
            // The compositor side dropped the channel: the shell is gone or
            // going. The sender is told, and the log says why a notification
            // that was accepted never appeared.
            tracing::error!("notification {id} could not be delivered: {e}");
            return Err(zbus::fdo::Error::Failed(format!(
                "the notification could not be delivered to the compositor: {e}"
            )));
        }
        Ok(id)
    }

    fn close_notification(
        &self,
        id: u32,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> Result<(), zbus::fdo::Error> {
        // A close of somebody else's notification is refused silently, in the
        // shape the specification gives a close of one that is not there: the
        // sender has no way to act on the difference, and an error reply
        // would only tell an attacker their guess was right.
        let sender = header.sender().map(|name| name.to_string());
        let owned = match self.owners.lock() {
            Ok(owners) => may_close(&owners, id, sender.as_deref()),
            Err(_) => true,
        };
        if !owned {
            tracing::debug!("close of notification {id} refused: the caller is not its owner");
            return Ok(());
        }

        // Same story as `notify`: a close request that vanishes into a dead
        // channel leaves the sender believing its notification is gone when
        // it is still on screen.
        self.sender.send(Message::Close(id)).map_err(|e| {
            tracing::error!("close of notification {id} could not be delivered: {e}");
            zbus::fdo::Error::Failed(format!(
                "the close request could not be delivered to the compositor: {e}"
            ))
        })
    }

    /// What this implementation supports. A sender checks these before using
    /// them, so claiming something absent is worse than claiming nothing.
    fn get_capabilities(&self) -> Vec<String> {
        let mut capabilities = vec![
            "actions".to_owned(),
            "body".to_owned(),
            // No "body-markup": the shell decides how a body is rendered, and
            // promising markup the stylesheet may not honour would show tags
            // as text.
            "persistence".to_owned(),
        ];
        // Only when there is something to play through. A session with no
        // sound server claiming "sound" is exactly the absent capability the
        // note above warns about: a sender that sets `suppress-sound` and
        // plays its own would go silent on the strength of the claim.
        if self.player.is_some() {
            capabilities.push("sound".to_owned());
        }
        capabilities
    }

    fn get_server_information(&self) -> (String, String, String, String) {
        (
            "viewport".to_owned(),
            "viewport".to_owned(),
            env!("CARGO_PKG_VERSION").to_owned(),
            // The specification version implemented, not this program's.
            "1.2".to_owned(),
        )
    }
}

/// Actions arrive as a flat list of alternating key and label.
///
/// A trailing key with no label is dropped rather than shown with an empty
/// one: a button with no text is a button nobody can use.
/// Seconds since the epoch, or zero if the clock is before it.
///
/// Zero is also what a notification built without a stamp carries, and the
/// shell draws both the same way — as no time rather than as 1970.
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

/// How many notifications are kept unless the configuration says otherwise.
///
/// A centre is read by scrolling, and the entries below the first screenful
/// are ones somebody is looking for rather than at. Fifty is a working day of
/// an ordinary desk and a few hundred kilobytes at the outside.
pub const DEFAULT_HISTORY: usize = 50;

/// What was notified, kept after the popup has gone.
///
/// A notification is a popup and then it is nothing: one that arrived over a
/// fullscreen game, or while the screens were blanked, was never seen and
/// cannot be gone back to. That is what every desktop's notification centre
/// is for — and what a second daemon is usually installed to keep, while the
/// only copy that ever existed was in this process, on its way to the shell.
///
/// So it is kept here rather than in the shell. The shell is a web page that
/// is restarted when it crashes and reloaded when its stylesheet changes, and
/// a history that lived there would be lost by both. This survives either,
/// because the page asks for it again on load — see `notification.list`.
///
/// Newest first, which is the order a centre shows them in.
pub struct History {
    entries: Vec<Notification>,
    limit: usize,
}

impl Default for History {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            limit: DEFAULT_HISTORY,
        }
    }
}

impl History {
    /// How many to keep. Zero turns the centre off and empties it, which is
    /// what a session that would rather keep no record asks for.
    ///
    /// Applied on reload as well as at startup, so lowering it takes the
    /// oldest entries away rather than waiting for them to age out.
    pub fn set_limit(&mut self, limit: usize) {
        self.limit = limit;
        self.trim();
    }

    pub fn entries(&self) -> &[Notification] {
        &self.entries
    }

    /// Record one, and say whether anything changed.
    ///
    /// A replacement — a sender reusing its own id for a progress bar, a chat
    /// window counting up — replaces the entry it names rather than stacking
    /// beside it, and moves to the top: it is news again. Without that a file
    /// copy would fill the centre by itself.
    pub fn record(&mut self, notification: &Notification) -> bool {
        if self.limit == 0 {
            return false;
        }
        self.entries.retain(|kept| kept.id != notification.id);
        self.entries.insert(0, notification.clone());
        self.trim();
        true
    }

    /// Drop one, and say whether it was there.
    pub fn forget(&mut self, id: u32) -> bool {
        let before = self.entries.len();
        self.entries.retain(|kept| kept.id != id);
        self.entries.len() != before
    }

    /// Drop all of them, and say whether there were any.
    pub fn clear(&mut self) -> bool {
        let had = !self.entries.is_empty();
        self.entries.clear();
        had
    }

    fn trim(&mut self) {
        if self.entries.len() > self.limit {
            self.entries.truncate(self.limit);
        }
    }
}

impl crate::state::ViewportState {
    /// Send the centre what it is drawing.
    ///
    /// Pushed on every change rather than polled, which is what
    /// `clipboard.history` does and for the same reason: the shell draws it
    /// only while the centre is open, and a message on a notification is not
    /// a message on a timer.
    pub fn publish_notification_history(&mut self) {
        let event = viewport_ipc::Event::NotificationHistory {
            entries: self.notification_history.entries().to_vec(),
        };
        self.notify(&event);
    }
}

fn parse_actions(flat: &[String]) -> Vec<NotificationAction> {
    flat.chunks_exact(2)
        .map(|pair| NotificationAction {
            key: pair[0].clone(),
            label: pair[1].clone(),
        })
        .collect()
}

/// Urgency, defaulting to normal.
///
/// The hint is a byte in the specification, but senders have been seen using
/// other integer types, so anything that converts is accepted.
fn urgency(hints: &HashMap<String, zvariant::OwnedValue>) -> u8 {
    hints
        .get("urgency")
        .and_then(|value| {
            u8::try_from(value)
                .ok()
                .or_else(|| u32::try_from(value).ok().and_then(|v| u8::try_from(v).ok()))
        })
        .unwrap_or(1)
}

/// What this notification should sound like, if anything.
///
/// Three hints, in the order the specification does *not* give — it lists
/// `sound-file`, `sound-name` and `suppress-sound` without saying how they
/// interact, so this picks:
///
/// * `suppress-sound` silences everything, including the configured default.
///   The hint means "I am playing my own", and a server that plays anyway is
///   two sounds for one event.
/// * `sound-file` before `sound-name`, matching what GNOME Shell does. A path
///   is unambiguous; a name resolves against the installed theme and may
///   resolve to nothing, so preferring the name would turn a sender that
///   helpfully sent both into silence on a machine with a thin theme.
/// * the configured default when a sender names neither, which is the case for
///   almost every notification on a desktop.
fn sound(
    hints: &HashMap<String, zvariant::OwnedValue>,
    default: Option<crate::sound::Sound>,
) -> Option<crate::sound::Sound> {
    if suppress_sound(hints) {
        return None;
    }
    crate::sound::Sound::from_config(hint_str(hints, "sound-file"), hint_str(hints, "sound-name"))
        .or(default)
}

/// A string hint, or nothing if it is absent or of another type.
fn hint_str<'a>(hints: &'a HashMap<String, zvariant::OwnedValue>, key: &str) -> Option<&'a str> {
    hints
        .get(key)
        .and_then(|value| <&str>::try_from(value).ok())
}

/// Whether the sender is playing its own sound and wants none from here.
///
/// A boolean in the specification, and read leniently for the same reason
/// [`urgency`] is: senders do send the other spellings, and the cost of
/// refusing them is a sender that asked for silence getting two sounds.
fn suppress_sound(hints: &HashMap<String, zvariant::OwnedValue>) -> bool {
    hints
        .get("suppress-sound")
        .and_then(|value| {
            bool::try_from(value).ok().or_else(|| {
                u32::try_from(value)
                    .ok()
                    .or_else(|| u8::try_from(value).ok().map(u32::from))
                    .map(|number| number != 0)
            })
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One notification, with only the fields the history cares about set.
    fn kept(id: u32, summary: &str) -> Notification {
        Notification {
            id,
            app_name: "test".to_owned(),
            icon: String::new(),
            summary: summary.to_owned(),
            body: String::new(),
            urgency: 1,
            timeout: -1,
            actions: Vec::new(),
            at: 1_700_000_000,
        }
    }

    #[test]
    fn the_history_is_newest_first() {
        let mut history = History::default();
        history.record(&kept(1, "first"));
        history.record(&kept(2, "second"));
        let ids: Vec<u32> = history.entries().iter().map(|n| n.id).collect();
        assert_eq!(ids, vec![2, 1]);
    }

    #[test]
    fn a_replacement_takes_the_place_of_what_it_replaces() {
        let mut history = History::default();
        history.record(&kept(1, "downloading"));
        history.record(&kept(2, "something else"));
        history.record(&kept(1, "downloaded"));

        // One entry for that sender, not two, and back at the top: it is news
        // again. A file copy counting up would otherwise fill the centre by
        // itself.
        let ids: Vec<u32> = history.entries().iter().map(|n| n.id).collect();
        assert_eq!(ids, vec![1, 2]);
        assert_eq!(history.entries()[0].summary, "downloaded");
    }

    #[test]
    fn the_oldest_go_when_the_limit_is_reached() {
        let mut history = History::default();
        history.set_limit(2);
        history.record(&kept(1, "one"));
        history.record(&kept(2, "two"));
        history.record(&kept(3, "three"));
        let ids: Vec<u32> = history.entries().iter().map(|n| n.id).collect();
        assert_eq!(ids, vec![3, 2]);
    }

    #[test]
    fn lowering_the_limit_drops_what_is_already_over_it() {
        // The reload case: a configuration that now keeps fewer has to take
        // the oldest away then and there, not wait for them to age out.
        let mut history = History::default();
        for id in 1..=5 {
            history.record(&kept(id, "x"));
        }
        history.set_limit(2);
        assert_eq!(history.entries().len(), 2);
    }

    #[test]
    fn a_limit_of_zero_keeps_nothing_and_empties_what_was_kept() {
        let mut history = History::default();
        history.record(&kept(1, "one"));
        history.set_limit(0);
        assert!(history.entries().is_empty());
        assert!(!history.record(&kept(2, "two")));
        assert!(history.entries().is_empty());
    }

    #[test]
    fn forgetting_says_whether_there_was_anything_to_forget() {
        let mut history = History::default();
        history.record(&kept(1, "one"));
        assert!(history.forget(1));
        // Twice is not an error: a dismissal and an expiry can both land on
        // the same id, and neither is worth a message to the shell.
        assert!(!history.forget(1));
        assert!(!history.clear());
    }

    #[test]
    fn a_stamp_is_carried_through() {
        // What the centre draws a time from. A popup does not need one; a list
        // of messages with no times on it is a list nobody can place.
        let mut history = History::default();
        history.record(&kept(1, "one"));
        assert_eq!(history.entries()[0].at, 1_700_000_000);
    }

    #[test]
    fn actions_pair_up_into_buttons() {
        let flat = vec![
            "default".to_owned(),
            "Open".to_owned(),
            "reply".to_owned(),
            "Reply".to_owned(),
        ];
        let actions = parse_actions(&flat);
        assert_eq!(actions.len(), 2);
        assert_eq!(actions[0].key, "default");
        assert_eq!(actions[0].label, "Open");
        assert_eq!(actions[1].key, "reply");
    }

    #[test]
    fn a_key_with_no_label_is_dropped() {
        // A button with no text is a button nobody can use, and senders do get
        // this wrong.
        let flat = vec!["default".to_owned(), "Open".to_owned(), "orphan".to_owned()];
        let actions = parse_actions(&flat);
        assert_eq!(
            actions.len(),
            1,
            "the orphan should not have become a button"
        );
    }

    #[test]
    fn no_actions_is_not_an_error() {
        assert!(parse_actions(&[]).is_empty());
    }

    /// A server with nowhere to send, for the id allocation alone.
    fn server() -> (
        Server,
        smithay::reexports::calloop::channel::Channel<Message>,
    ) {
        let (sender, channel) = smithay::reexports::calloop::channel::channel();
        let server = Server {
            sender,
            next: Arc::new(AtomicU32::new(1)),
            owners: Arc::new(Mutex::new(HashMap::new())),
            default_sound: Arc::new(Mutex::new(None)),
            // No sound server under a test runner, and none wanted: these
            // cover id allocation. What plays is [`sound`]'s, tested directly.
            player: None,
        };
        (server, channel)
    }

    /// A header for a local call, with or without a sender on it.
    ///
    /// The bus stamps the real one on when the message arrives; building one
    /// here is what lets these tests exercise ownership without a bus. The
    /// message leaks so the header can borrow from it for `'static`.
    fn header(sender: Option<&str>) -> zbus::message::Header<'static> {
        let builder = zbus::Message::method_call("/org/freedesktop/Notifications", "Notify")
            .and_then(|builder| match sender {
                Some(sender) => builder.sender(sender),
                None => Ok(builder),
            })
            .and_then(|builder| builder.build(&()))
            .expect("a message to hang a header off");
        Box::leak(Box::new(builder)).header()
    }

    fn notify_as(server: &Server, replaces_id: u32, sender: Option<&str>) -> u32 {
        server
            .notify(
                "test".to_owned(),
                replaces_id,
                String::new(),
                "summary".to_owned(),
                String::new(),
                Vec::new(),
                HashMap::new(),
                -1,
                header(sender),
            )
            // The channel is alive under a test runner; delivery is what the
            // error return is for, and id allocation is what these cover.
            .expect("the compositor side of the channel is alive")
    }

    fn notify(server: &Server, replaces_id: u32) -> u32 {
        notify_as(server, replaces_id, None)
    }

    #[test]
    fn a_replaced_notification_keeps_its_id() {
        let (server, _channel) = server();
        assert_eq!(notify(&server, 0), 1);
        assert_eq!(notify(&server, 1), 1, "a replacement updates in place");
    }

    #[test]
    fn an_id_a_sender_chose_is_never_handed_out_again() {
        // Both ends allocate ids. A `replaces_id` honoured without moving the
        // counter past it came back later as a fresh id, and then two live
        // notifications had one id — dismissing either closed the other.
        let (server, _channel) = server();
        assert_eq!(notify(&server, 7), 7);
        let fresh = notify(&server, 0);
        assert!(fresh > 7, "the counter should be past 7, not at {fresh}");
    }

    #[test]
    fn a_replacement_of_an_older_id_does_not_rewind_the_counter() {
        let (server, _channel) = server();
        assert_eq!(notify(&server, 7), 7);
        assert_eq!(notify(&server, 2), 2);
        assert!(notify(&server, 0) > 7, "fetch_max, not a store");
    }

    /// Replacement is the owner's alone. A sender that guesses another
    /// application's id gets a fresh notification of its own — and the id it
    /// tried to take stays where it was.
    #[test]
    fn a_stranger_cannot_replace_another_application_s_notification() {
        let (server, _channel) = server();
        assert_eq!(notify_as(&server, 0, Some(":1.2")), 1);
        // The owner replaces its own; the stranger does not.
        assert_eq!(notify_as(&server, 1, Some(":1.2")), 1);
        assert_ne!(notify_as(&server, 1, Some(":1.9")), 1);
        assert_eq!(
            server.owners.lock().unwrap().get(&1).map(String::as_str),
            Some(":1.2")
        );
    }

    /// An id nobody holds is not trusted either: a fresh one is allocated,
    /// which is mako's rule — a sender may only replace what it owns.
    #[test]
    fn an_unknown_replaces_id_is_a_fresh_notification() {
        let (server, _channel) = server();
        assert_ne!(notify_as(&server, 9, Some(":1.2")), 9);
    }

    /// Refusing the hijack must not disturb the counter: fresh ids keep
    /// coming one after another, and the refused id is never handed out as
    /// if it were fresh while its owner's notification is still live.
    #[test]
    fn a_refused_replaces_id_does_not_disturb_allocation() {
        let (server, _channel) = server();
        assert_eq!(notify_as(&server, 0, Some(":1.2")), 1);
        // A stranger guessing high: refused, and the counter does not jump.
        notify_as(&server, 4_000_000_000, Some(":1.9"));
        assert_eq!(notify_as(&server, 0, Some(":1.3")), 3);
    }

    /// Closing is the owner's alone, and an unowned id stays closable by
    /// anyone, which is what it was before owners were tracked at all.
    #[test]
    fn close_is_the_owner_s_alone() {
        let mut owners = HashMap::new();
        owners.insert(4u32, ":1.2".to_owned());
        assert!(may_close(&owners, 4, Some(":1.2")));
        assert!(!may_close(&owners, 4, Some(":1.9")));
        assert!(may_close(&owners, 5, Some(":1.9")), "an unknown id");
        assert!(may_close(&owners, 4, None), "no sender to check against");

        // And through the server itself, end to end: a stranger's close is
        // refused without error and the compositor never hears of it, while
        // the owner's own close goes through.
        let (server, channel) = server();
        notify_as(&server, 0, Some(":1.2"));
        // The notify above put its notification on the channel; that much was
        // supposed to arrive, so it is drained before the closes are judged.
        while let Ok(message) = channel.try_recv() {
            assert!(
                matches!(message, Message::Add(_)),
                "only the notification itself was meant to be in flight"
            );
        }
        server
            .close_notification(1, header(Some(":1.9")))
            .expect("a silent refusal is not an error");
        assert!(
            matches!(
                channel.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ),
            "nothing reached the compositor"
        );
        server
            .close_notification(1, header(Some(":1.2")))
            .expect("the owner may close");
        assert!(matches!(channel.try_recv(), Ok(Message::Close(1))));
    }

    #[test]
    fn urgency_defaults_to_normal() {
        // Absent means normal, per the specification — not zero, which is low
        // and would make every notification from a well-behaved sender quiet.
        assert_eq!(urgency(&HashMap::new()), 1);
    }

    #[test]
    fn urgency_is_read_from_the_hint() {
        let mut hints = HashMap::new();
        hints.insert("urgency".to_owned(), zvariant::OwnedValue::from(2u8));
        assert_eq!(urgency(&hints), 2);
    }

    /// The configured default, for the tests that check it is or is not used.
    fn default_sound() -> Option<crate::sound::Sound> {
        Some(crate::sound::Sound::File("/sounds/bark.ogg".to_owned()))
    }

    fn hints(pairs: &[(&str, zvariant::OwnedValue)]) -> HashMap<String, zvariant::OwnedValue> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.try_clone().unwrap()))
            .collect()
    }

    #[test]
    fn a_notification_with_no_sound_hints_plays_the_configured_one() {
        // Which is almost every notification on a desktop: `notify-send` sets
        // none of these.
        assert_eq!(sound(&HashMap::new(), default_sound()), default_sound());
    }

    #[test]
    fn no_hints_and_no_default_is_silence() {
        assert_eq!(sound(&HashMap::new(), None), None);
    }

    #[test]
    fn a_sound_file_hint_is_played_instead_of_the_default() {
        let hints = hints(&[(
            "sound-file",
            zvariant::OwnedValue::from(zvariant::Str::from("/sounds/chime.ogg")),
        )]);
        assert_eq!(
            sound(&hints, default_sound()),
            Some(crate::sound::Sound::File("/sounds/chime.ogg".to_owned()))
        );
    }

    #[test]
    fn a_sound_name_hint_is_played_instead_of_the_default() {
        let hints = hints(&[(
            "sound-name",
            zvariant::OwnedValue::from(zvariant::Str::from("message-new-instant")),
        )]);
        assert_eq!(
            sound(&hints, default_sound()),
            Some(crate::sound::Sound::Name("message-new-instant".to_owned()))
        );
    }

    #[test]
    fn a_file_hint_wins_over_a_name_hint() {
        // The specification gives no order. A path always resolves; a name
        // resolves against whatever theme is installed, so preferring the name
        // would turn a sender that helpfully sent both into silence.
        let hints = hints(&[
            (
                "sound-file",
                zvariant::OwnedValue::from(zvariant::Str::from("/sounds/chime.ogg")),
            ),
            (
                "sound-name",
                zvariant::OwnedValue::from(zvariant::Str::from("bell")),
            ),
        ]);
        assert_eq!(
            sound(&hints, None),
            Some(crate::sound::Sound::File("/sounds/chime.ogg".to_owned()))
        );
    }

    #[test]
    fn suppress_sound_silences_the_configured_default() {
        // The whole point of the hint: the sender is playing its own, and a
        // server that plays anyway is two sounds for one event.
        let hints = hints(&[("suppress-sound", zvariant::OwnedValue::from(true))]);
        assert_eq!(sound(&hints, default_sound()), None);
    }

    #[test]
    fn suppress_sound_silences_a_sound_the_sender_itself_named() {
        // Senders do send both — a library sets `sound-name` and the
        // application then asks for quiet.
        let hints = hints(&[
            ("suppress-sound", zvariant::OwnedValue::from(true)),
            (
                "sound-name",
                zvariant::OwnedValue::from(zvariant::Str::from("bell")),
            ),
        ]);
        assert_eq!(sound(&hints, None), None);
    }

    #[test]
    fn suppress_sound_set_false_leaves_the_sound_alone() {
        let hints = hints(&[("suppress-sound", zvariant::OwnedValue::from(false))]);
        assert_eq!(sound(&hints, default_sound()), default_sound());
    }

    #[test]
    fn a_suppress_sound_sent_as_an_integer_still_reads() {
        // As with urgency: the specification says boolean and senders send
        // integers. Refusing means a sender that asked for silence gets two
        // sounds, which is the failure the hint exists to prevent.
        let hints = hints(&[("suppress-sound", zvariant::OwnedValue::from(1u32))]);
        assert_eq!(sound(&hints, default_sound()), None);
    }

    #[test]
    fn a_sound_hint_of_the_wrong_type_falls_back_rather_than_failing() {
        // A number where a path belongs is a broken sender, not a reason to
        // drop the notification's sound entirely.
        let hints = hints(&[("sound-file", zvariant::OwnedValue::from(7u32))]);
        assert_eq!(sound(&hints, default_sound()), default_sound());
    }

    #[test]
    fn an_urgency_sent_as_the_wrong_integer_still_reads() {
        // The specification says a byte. Senders have been seen using u32, and
        // refusing would silently make every one of their notifications
        // normal.
        let mut hints = HashMap::new();
        hints.insert("urgency".to_owned(), zvariant::OwnedValue::from(2u32));
        assert_eq!(urgency(&hints), 2);
    }
}
