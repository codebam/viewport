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

/// Whether this instance should ask for one of the session-wide names at all.
///
/// The wide names — the portal backend name, `org.freedesktop.Notifications`,
/// `org.freedesktop.ScreenSaver`, `org.kde.StatusNotifierWatcher` — answer for
/// the whole session, so only the compositor the user is looking at should
/// hold them. That is the one on DRM, the same rule `main.rs` already applies
/// to `export_session_environment`, which this extends: a headless or nested
/// instance is a client of the session around it and answers for no desktop.
///
/// A leftover `--headless` test instance is what showed the rule is needed. It
/// sat on the session bus — nothing had given it a bus of its own — and held
/// the portal name for hours. Every screen share from the session's own
/// applications was then routed to it, where an abandoned test had left its
/// session locked, and each one was refused there ("screencast: the session is
/// locked") while the compositor the user was looking at sat queued behind it,
/// saying so at INFO once and never asked again. A name queued behind a dead
/// holder comes back by itself; one queued behind a live holder that never
/// exits does not.
///
/// The claims a test makes on its own bus are the point of the tests, and
/// stay: a private bus is one where this instance IS the desktop, and the
/// integration tests grep for `claimed ...` in the log to see it happen.
pub fn may_claim(session_compositor: bool) -> bool {
    serves(session_compositor, &session_bus_address())
}

/// The decision itself, with the bus passed in so it is tested without
/// touching the environment of the process running the tests.
fn serves(session_compositor: bool, bus_address: &str) -> bool {
    session_compositor || !is_session_bus(bus_address)
}

/// Whether an address is the session bus of this login — the one logind puts
/// at `/run/user/UID/bus`, and the one every desktop process shares.
///
/// A bus a test makes for itself is anywhere else at all, so the path is the
/// whole test. An `abstract=` address can never be one: logind's socket is a
/// file, and so is the fallback zbus's session builder takes, which
/// [`session_bus_address`] mirrors.
fn is_session_bus(bus_address: &str) -> bool {
    let path = bus_address
        .split("path=")
        .nth(1)
        .map(|rest| rest.split(',').next().unwrap_or(rest));
    matches!(path, Some(path) if path.starts_with("/run/user/") && path.ends_with("/bus"))
}

/// What zbus's session-bus builder will connect to: `DBUS_SESSION_BUS_ADDRESS`
/// if the environment names one, else the logind socket under
/// `XDG_RUNTIME_DIR`, which is the same fallback the builder takes.
fn session_bus_address() -> String {
    match std::env::var_os("DBUS_SESSION_BUS_ADDRESS") {
        Some(address) => address.to_string_lossy().into_owned(),
        None => format!(
            "{}/bus",
            std::env::var_os("XDG_RUNTIME_DIR")
                .unwrap_or_default()
                .to_string_lossy()
        ),
    }
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

/// Say why a name is not being asked for, once per name.
///
/// On the same half of the log [`log_name_reply`] speaks for: silence here
/// would read exactly like a claim.
pub fn log_name_skipped(name: &str) {
    tracing::warn!(
        "not claiming {name}: this instance is not the session's own compositor, \
         and a name held here answers for a desktop nobody is looking at"
    );
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

    /// The claims a test makes on its own bus are the tests — a private bus is
    /// one where this instance IS the desktop — so a run with no DRM behind it
    /// still claims there.
    #[test]
    fn a_bus_of_ones_own_is_claimed_on() {
        assert!(serves(
            false,
            "unix:path=/tmp/dbus-FsaaxRxMnf,guid=67d82851c8ddc18a05c771e86ad193f3"
        ));
        assert!(serves(
            false,
            "unix:abstract=/tmp/dbus-kZ0mQz,guid=67d82851c8ddc18a05c771e86ad193f3"
        ));
    }

    /// The session bus belongs to the session's compositor, and only to it: a
    /// headless or nested instance answering there is a leftover test holding
    /// the screen shares, notifications and idle holds of a desktop it cannot
    /// see — which is how every screen share was refused for hours by an
    /// instance whose own stuck lock screen was the thing answering.
    #[test]
    fn the_session_bus_is_left_to_the_session_compositor() {
        assert!(!serves(false, "unix:path=/run/user/1000/bus"));
        assert!(!serves(
            false,
            "unix:path=/run/user/1000/bus,guid=67d82851c8ddc18a05c771e86ad193f3"
        ));
        assert!(serves(true, "unix:path=/run/user/1000/bus"));
    }

    /// The path is the whole test: a socket somewhere else is nobody's login
    /// session, however it is named.
    #[test]
    fn only_loginds_own_bus_counts_as_the_session_bus() {
        assert!(is_session_bus("unix:path=/run/user/1000/bus"));
        assert!(is_session_bus("unix:path=/run/user/1234/bus,guid=1234"));
        assert!(!is_session_bus("unix:path=/run/user/1000/bus-something"));
        assert!(!is_session_bus("unix:path=/run/user/1000/wayland-1"));
        assert!(!is_session_bus("unix:path=/tmp/dbus-abc"));
        assert!(!is_session_bus("unix:abstract=/tmp/dbus-abc"));
    }
}
