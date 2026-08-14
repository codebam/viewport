// SPDX-License-Identifier: GPL-3.0-or-later
//
// How this compositor asks for a name on the session bus.
//
// Two names are claimed — `org.freedesktop.Notifications` and
// `org.freedesktop.impl.portal.desktop.viewport` — and both are claimed the
// same way, which is why the rule lives here rather than twice.

/// The flags every bus name is requested with.
///
/// zbus's default is `AllowReplacement | ReplaceExisting | DoNotQueue`, and two
/// of those three are wrong for a compositor.
///
/// **`DoNotQueue` is the one that broke a session.** It does not only mean
/// "fail rather than wait": it also decides what happens *after* being
/// replaced, and with it set a replaced owner is dropped instead of being put
/// back in the queue. A second compositor was started nested — someone trying
/// a shell backend out — and took both names from the session around it; when
/// that nested run ended, both names were owned by nobody at all. Every
/// notification on that desktop then failed with
///
///   GDBus.Error:org.freedesktop.DBus.Error.ServiceUnknown:
///   The name is not activatable
///
/// while the compositor sat there serving the interface, never asked again,
/// and the portal backend was gone with it. Queued, both come back the moment
/// the other process lets go.
///
/// **`ReplaceExisting` is wrong for the same reason from the other end.** It
/// is what let the nested run take the names in the first place, and it is
/// what `appearance.rs` has always said it did not do: "the name is claimed if
/// it is free and left alone if it is not, rather than replacing whoever holds
/// it — a real desktop portal running alongside knows more about the session
/// than this does". That was true of the comment and not of the code. There is
/// no case that needs it either: a name held by a process that died is
/// released by the bus when its connection closes, so there are no stale
/// owners to displace.
///
/// **`AllowReplacement` stays.** A notification daemon somebody starts on
/// purpose should win; this compositor serves the interface, it does not own
/// the idea. Losing the name that way is now a wait rather than a goodbye.
pub fn name_flags() -> enumflags2::BitFlags<zbus::fdo::RequestNameFlags> {
    zbus::fdo::RequestNameFlags::AllowReplacement.into()
}

/// Say what came of asking, in the log, once per name.
///
/// `InQueue` is not a failure and is not rare — it is what a second compositor
/// on the same bus should see — so it is worth a line that says so rather than
/// silence that reads as "claimed".
pub fn log_name_reply(name: &str, reply: zbus::Result<zbus::fdo::RequestNameReply>) {
    match reply {
        Ok(zbus::fdo::RequestNameReply::PrimaryOwner) => tracing::info!("claimed {name}"),
        Ok(zbus::fdo::RequestNameReply::InQueue) => {
            tracing::info!("{name} is held by another program; queued for it")
        }
        Ok(other) => tracing::warn!("{name}: {other:?}"),
        Err(e) => tracing::warn!("could not ask for {name}: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::fdo::RequestNameFlags::{AllowReplacement, DoNotQueue, ReplaceExisting};

    /// A compositor that is replaced waits for the name rather than forgetting
    /// it, and never takes one from whoever already has it.
    ///
    /// Both halves of the session this comes from: a nested run took the
    /// notification daemon and the portal backend from the desktop around it,
    /// and when it ended neither name had an owner at all.
    #[test]
    fn a_name_is_queued_for_and_never_taken() {
        let flags = name_flags();
        assert!(
            !flags.contains(DoNotQueue),
            "being replaced once would mean never holding the name again"
        );
        assert!(
            !flags.contains(ReplaceExisting),
            "whoever holds it now knows more about this session than we do"
        );
        assert!(
            flags.contains(AllowReplacement),
            "a daemon somebody started on purpose should win"
        );
    }
}
