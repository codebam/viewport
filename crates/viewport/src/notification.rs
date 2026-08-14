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
use std::sync::Arc;

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
}

impl Default for Notifications {
    fn default() -> Self {
        Self {
            outbound: None,
            // Zero means "allocate one" in the protocol, so ids start at one.
            next: Arc::new(AtomicU32::new(1)),
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
            replaces_id
        } else {
            self.next.fetch_add(1, Ordering::Relaxed)
        };

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
        vec![
            "actions".to_owned(),
            "body".to_owned(),
            // No "body-markup": the shell decides how a body is rendered, and
            // promising markup the stylesheet may not honour would show tags
            // as text.
            "persistence".to_owned(),
        ]
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
