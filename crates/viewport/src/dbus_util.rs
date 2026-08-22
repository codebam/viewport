// SPDX-License-Identifier: GPL-3.0-or-later
//
// Shared plumbing for the D-Bus service threads.
//
// The tray, MPRIS, NetworkManager and BlueZ workers are four copies of one
// shape: a thread on a connection of its own, a channel of commands back to
// the compositor, and one more thread per match rule turning signals into
// commands. The pieces below are the parts that were written out more than
// once, gathered so the next service starts from one copy rather than a
// fifth.

use std::sync::mpsc;

/// Run one piece of I/O on a throwaway thread, and wait with a stopwatch.
///
/// zbus's blocking calls take no deadline, so the deadline is taken around
/// them. `None` means the call outlasted `timeout`; the thread it was handed
/// to goes on trying in the background and is collected by the connection's
/// own method timeout, while this one gets on with the rest of its queue.
/// That makes the loser of the race a bounded leak rather than an unbounded
/// one, which is the best a blocking API offers.
pub fn with_deadline<T: Send + 'static>(
    timeout: std::time::Duration,
    name: &str,
    io: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let (done, answered) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name(format!("dbus-{name}"))
        .spawn(move || {
            let _ = done.send(io());
        })
        .ok()?;
    answered.recv_timeout(timeout).ok()
}

/// One thread reading one match rule, turning messages into commands.
///
/// Its own thread rather than a dispatch on the worker's, because the worker
/// spends its time blocked on method calls that can take seconds — pairing,
/// associating with an access point — and a signal that arrives during one of
/// those must not be dropped.
pub fn pump<C: Send + 'static>(
    connection: zbus::blocking::Connection,
    commands: mpsc::Sender<C>,
    thread: &str,
    rule: String,
    handle: fn(&zbus::Message, &mpsc::Sender<C>),
) -> anyhow::Result<()> {
    let rule = zbus::MatchRule::try_from(rule.as_str())?;
    let messages = zbus::blocking::MessageIterator::for_match_rule(rule, &connection, None)?;
    std::thread::Builder::new()
        .name(thread.to_owned())
        .spawn(move || {
            for message in messages.flatten() {
                handle(&message, &commands);
            }
        })?;
    Ok(())
}

/// The half of a bus error that was written for a person.
///
/// zbus's `Display` for a method error is the bus name of the error followed
/// by its message — `org.freedesktop.NetworkManager.Error.…: Secrets were
/// required…` — which is a sentence with a fully qualified Java class in the
/// middle of it. The message alone is the part written for a person.
pub fn complaint<E: Into<zbus::Error>>(error: E) -> String {
    // Generic over the two error types zbus hands back, which are the same
    // failure told twice: a method call fails with `zbus::Error` and a
    // property write with `zbus::fdo::Error`, because setting a property is a
    // call to `org.freedesktop.DBus.Properties.Set`. One conversion rather
    // than two spellings of this function.
    let error: zbus::Error = error.into();
    match &error {
        zbus::Error::MethodError(_, Some(message), _) => message.clone(),
        _ => error.to_string(),
    }
}
