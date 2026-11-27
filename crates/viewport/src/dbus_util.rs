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

use zbus::{OwnedVariant, Variant};

/// A value the way D-Bus carries it.
///
/// Infallible for everything here: only file descriptors can fail to be owned,
/// and a setting, a shortcut description, or a signal body is never one.
pub fn owned<'a, T: Into<Variant<'a>>>(value: T) -> OwnedVariant {
    value
        .into()
        .try_to_owned()
        .expect("a D-Bus value is never a file descriptor")
}

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
    connection: crate::dbus::sync::Connection,
    commands: mpsc::Sender<C>,
    thread: &str,
    rule: String,
    handle: fn(&zbus::Message, &mpsc::Sender<C>),
) -> anyhow::Result<()> {
    let rule = zbus::MatchRule::try_from(rule.as_str())?.to_owned();
    let messages = crate::dbus::sync::MessageIterator::for_match_rule(rule, &connection, None)?;
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
    use zbus::DBusError as _;

    // Generic over the two error types zbus hands back, which are the same
    // failure told twice: a method call fails with `zbus::Error` and a
    // property write with `zbus::fdo::Error`, because setting a property is a
    // call to `org.freedesktop.DBus.Properties.Set`. One conversion rather
    // than two spellings of this function.
    //
    // `Into<zbus::Error>` wraps `zbus::fdo::Error` in `zbus::Error::FDO(...)`,
    // not `MethodError`, so unwrap the `FDO` arm too (including nested
    // `fdo::Error::ZBus(MethodError)`).
    let error: zbus::Error = error.into();
    match &error {
        zbus::Error::MethodError(_, Some(message), _) => message.clone(),
        zbus::Error::FDO(fdo) => match fdo.as_ref() {
            zbus::fdo::Error::ZBus(inner) => match inner {
                zbus::Error::MethodError(_, Some(message), _) => message.clone(),
                _ => inner.to_string(),
            },
            other => other
                .description()
                .map(str::to_owned)
                .unwrap_or_else(|| other.to_string()),
        },
        _ => error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn owned_converts_primitive_values() {
        let value = owned(42u32);
        assert_eq!(u32::try_from(value).expect("u32"), 42);
        let text = owned("viewport");
        assert_eq!(String::try_from(text).expect("string"), "viewport");
    }

    #[test]
    fn with_deadline_returns_result_before_timeout_and_none_on_expiry() {
        let fast = with_deadline(Duration::from_secs(1), "fast", || 7usize);
        assert_eq!(fast, Some(7));

        let slow = with_deadline(Duration::from_millis(20), "slow", || {
            std::thread::sleep(Duration::from_millis(200));
            99usize
        });
        assert_eq!(slow, None);
    }

    #[test]
    fn complaint_extracts_method_error_message_and_falls_back_otherwise() {
        let fdo = zbus::fdo::Error::Failed("Secrets were required".to_owned());
        assert_eq!(complaint(fdo), "Secrets were required");

        let other = zbus::Error::Unsupported;
        assert_eq!(complaint(other), zbus::Error::Unsupported.to_string());
    }
}
