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
        let server = Server {
            sender,
            next: next.clone(),
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
    pub fn invoke_action(&self, id: u32, action: &str) {
        self.emit("ActionInvoked", &(id, action));
    }

    /// Tell the sender its notification is gone, and why.
    pub fn closed(&self, id: u32, reason: CloseReason) {
        self.emit("NotificationClosed", &(id, reason as u32));
    }

    fn emit<T: serde::Serialize + zvariant::DynamicType>(&self, signal: &str, body: &T) {
        let Some(connection) = self.outbound.as_ref() else {
            return;
        };
        if let Err(e) = connection.emit_signal(
            None::<&str>,
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
    default_sound: Arc<Mutex<Option<crate::sound::Sound>>>,
    /// Absent when there is no sound server to play through.
    player: Option<crate::sound::Player>,
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl Server {
    /// The one method that matters. Returns the id the sender should use to
    /// replace or close this notification later.
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
    ) -> u32 {
        // A sender replacing its own notification keeps the id, so it updates
        // in place rather than stacking a duplicate.
        let id = if replaces_id != 0 {
            // And the counter is moved past it. Both ends allocate ids, and a
            // replacement id honoured but not accounted for was handed out
            // again later as if it were fresh: two live notifications with one
            // id, and dismissing either one closed the other.
            self.next
                .fetch_max(replaces_id.saturating_add(1), Ordering::Relaxed);
            replaces_id
        } else {
            self.next.fetch_add(1, Ordering::Relaxed)
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
        };
        let _ = self.sender.send(Message::Add(Box::new(notification)));
        id
    }

    fn close_notification(&self, id: u32) {
        let _ = self.sender.send(Message::Close(id));
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
            default_sound: Arc::new(Mutex::new(None)),
            // No sound server under a test runner, and none wanted: these
            // cover id allocation. What plays is [`sound`]'s, tested directly.
            player: None,
        };
        (server, channel)
    }

    fn notify(server: &Server, replaces_id: u32) -> u32 {
        server.notify(
            "test".to_owned(),
            replaces_id,
            String::new(),
            "summary".to_owned(),
            String::new(),
            Vec::new(),
            HashMap::new(),
            -1,
        )
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
