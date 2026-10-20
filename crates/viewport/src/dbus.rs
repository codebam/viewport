// SPDX-License-Identifier: GPL-3.0-or-later
//
// How this compositor asks for a name on the session bus.
//
// Two names are claimed — `org.freedesktop.Notifications` and
// `org.freedesktop.impl.portal.desktop.viewport` — and both are claimed the
// same way, which is why the rule lives here rather than twice.

/// The synchronous face on zbus 6 that the service threads are written against.
///
/// zbus 5 shipped a blocking API — `crate::dbus::sync::Connection`, `*Blocking`
/// proxies, `MessageIterator` — and this compositor is built on it: the tray,
/// MPRIS, NetworkManager, BlueZ, the three portal backends and the notification
/// daemon each own a thread, that thread calls the bus and waits, and signals
/// arrive on one more thread per match rule. zbus 6 removed the module, and its
/// migration guide's answer ("put the program in one `block_on`") does not apply
/// here: the threads are the point — a round trip to a wedged bus daemon must
/// not stall a frame, and a served interface's method may not block on a call
/// of its own, which `zbus::block_on` panics on because the connection only
/// makes progress on the thread that is waiting.
///
/// What replaces it is [`block_on`], which runs the calling thread's own
/// zruntime alongside the future it is given. Called once per operation on the
/// thread that owns the connection, that reproduces the blocking API's
/// behaviour, and it is all the types below do. They own no runtime and start
/// no thread: a `block_on` per call is the whole mechanism.
///
/// **The one rule.** `block_on` panics when called from inside a future that
/// is already inside a `block_on`, and a served interface's method is exactly
/// that: zbus is driving it, on the thread the connection makes progress on,
/// and a call that waited there would be waiting for itself. So a method zbus
/// dispatches is `async` and `await`s its bus calls, never `block_on`s them —
/// which is why `inhibit::called_by_frontend` and the tray watcher build a
/// `DBusProxy` with `.await` while the worker threads beside them use the
/// synchronous helpers here. This is not a style preference; getting it wrong
/// panics the dispatch task and the interface stops answering. It was found
/// by `tests/tray.test.sh`, which failed four checks against an
/// `org.kde.StatusNotifierWatcher` that had stopped replying.
///
/// This submodule keeps the workers' shape rather than being a library. When a
/// worker is moved onto the async API directly, its use of this goes.
pub mod sync {
    use zbus::message::Message;
    use zbus::MessageStream;

    /// A session or system bus connection.
    ///
    /// The only difference from the blocking type it stands in for is that it
    /// is `zbus::Connection` underneath, which is what makes an async call
    /// possible when one is wanted. It is a type alias rather than a newtype so
    /// that `object_server`, `fdo` helpers and a served interface's
    /// `emitter.connection()` all take it without conversion; the constructors
    /// the blocking API had on it are the free functions below, because an
    /// inherent async `session` cannot be shadowed from outside the crate.
    pub type Connection = zbus::Connection;

    /// The system bus, synchronously — `zbus::blocking::Connection::system`.
    ///
    /// The session bus needs a builder here (every user serves something or
    /// sets a timeout on it), so it has no free function of its own.
    pub fn system() -> zbus::Result<zbus::Connection> {
        block_on(zbus::Connection::system())
    }

    /// Run a future to completion on this thread, for a connection that lives
    /// on it.
    ///
    /// This is the whole of the mechanism, and the reason a worker may treat
    /// its thread as synchronous. It must not be called from inside a future
    /// that is already inside a `block_on` — a served interface's method —
    /// because the connection would then be waiting on the very thread that
    /// called it; served methods are `async` and `await` instead.
    pub fn block_on<F: std::future::Future>(future: F) -> F::Output {
        zbus::block_on(future)
    }

    /// A proxy to one remote object, for use from a synchronous thread.
    ///
    /// `Clone`, so a worker can keep one and hand another to a match-rule
    /// thread. Every call drives the caller's own `block_on`, so two threads
    /// calling the same clone overlap rather than queue — as the blocking API
    /// allowed.
    pub type Proxy<'a> = zbus::Proxy<'a>;

    /// Build a proxy, the way `crate::dbus::sync::proxy::Builder` did.
    pub mod proxy {
        pub use zbus::proxy::Builder;

        /// `Builder::build`, synchronously.
        ///
        /// zbus 6's proxy builder sets its destination, path and interface
        /// infallibly and reports the first failure from `build`, which is
        /// async. This is that call, driven on the calling thread, so the six
        /// workers keep the `…build().ok()` shape they had.
        pub trait BuildBlocking<'a> {
            fn build_blocking(self) -> zbus::Result<zbus::Proxy<'a>>;
        }

        impl<'a> BuildBlocking<'a> for Builder<'a, zbus::Proxy<'a>> {
            fn build_blocking(self) -> zbus::Result<zbus::Proxy<'a>> {
                crate::dbus::sync::block_on(self.build())
            }
        }
    }

    /// Build a connection, the way `zbus::blocking::connection::Builder` did.
    pub mod connection {
        pub use zbus::connection::Builder;

        /// `Builder::build`, synchronously.
        ///
        /// zbus 6's connection builder is async end to end — `session()` and
        /// `system()` included — and its setters defer their errors to
        /// `build()`. This drives the whole thing on the calling thread, so the
        /// workers keep `Builder::session()?.serve_at(…)?.build()?`.
        pub trait BuildBlocking {
            fn build_blocking(self) -> zbus::Result<zbus::Connection>;
        }

        impl BuildBlocking for Builder<'static> {
            fn build_blocking(self) -> zbus::Result<zbus::Connection> {
                crate::dbus::sync::block_on(self.build())
            }
        }
    }

    /// `Connection::emit_signal`, synchronously.
    ///
    /// Async in zbus 6 because a send can wait for the socket; the workers call
    /// it from their own threads, so it is driven there with `block_on`.
    pub trait EmitSignalExt {
        /// Emit one signal. Named for what it does rather than taking the
        /// async call's name, which cannot be shadowed from outside zbus.
        #[allow(clippy::too_many_arguments)]
        fn emit_blocking<'a, D, P, I, M, B>(
            &self,
            destination: Option<D>,
            path: P,
            interface: I,
            signal_name: M,
            body: &B,
        ) -> zbus::Result<()>
        where
            D: TryInto<zbus::names::BusName<'a>>,
            P: TryInto<zbus::ObjectPath<'a>>,
            I: TryInto<zbus::names::InterfaceName<'a>>,
            M: TryInto<zbus::names::MemberName<'a>>,
            D::Error: Into<zbus::Error>,
            P::Error: Into<zbus::Error>,
            I::Error: Into<zbus::Error>,
            M::Error: Into<zbus::Error>,
            B: serde::ser::Serialize + zbus::DynamicType;
    }

    impl EmitSignalExt for zbus::Connection {
        fn emit_blocking<'a, D, P, I, M, B>(
            &self,
            destination: Option<D>,
            path: P,
            interface: I,
            signal_name: M,
            body: &B,
        ) -> zbus::Result<()>
        where
            D: TryInto<zbus::names::BusName<'a>>,
            P: TryInto<zbus::ObjectPath<'a>>,
            I: TryInto<zbus::names::InterfaceName<'a>>,
            M: TryInto<zbus::names::MemberName<'a>>,
            D::Error: Into<zbus::Error>,
            P::Error: Into<zbus::Error>,
            I::Error: Into<zbus::Error>,
            M::Error: Into<zbus::Error>,
            B: serde::ser::Serialize + zbus::DynamicType,
        {
            block_on(zbus::Connection::emit_signal(
                self,
                destination,
                path,
                interface,
                signal_name,
                body,
            ))
        }
    }

    /// A request to take or release a bus name, synchronously.
    ///
    /// The interface these two are generated for is a client of zbus's own
    /// `fdo` proxies, so it is here rather than in the workers.
    pub trait NameRequestExt {
        /// Ask the bus for a name, with the flags this compositor uses.
        ///
        /// Named for the call it drives rather than taking the async one's
        /// name, which cannot be shadowed from outside zbus.
        fn take_name(
            &self,
            name: &str,
            flags: zbus::fdo::RequestNameFlags,
        ) -> zbus::Result<zbus::fdo::RequestNameReply>;

        /// Give a name back.
        fn drop_name(&self, name: &str) -> zbus::Result<bool>;
    }

    impl NameRequestExt for zbus::Connection {
        fn take_name(
            &self,
            name: &str,
            flags: zbus::fdo::RequestNameFlags,
        ) -> zbus::Result<zbus::fdo::RequestNameReply> {
            block_on(zbus::Connection::request_name_with_flags(self, name, flags))
        }

        fn drop_name(&self, name: &str) -> zbus::Result<bool> {
            block_on(zbus::Connection::release_name(self, name))
        }
    }

    /// The synchronous calls of `zbus::blocking::Proxy`.
    ///
    /// zbus 6's `Proxy` is async only, while every worker wrote
    /// `proxy.get_property` and `proxy.call` as if it were not. Those are
    /// provided here with the signatures they had, each one a `block_on` of the
    /// async call, so a caller imports this trait and nothing else changes.
    pub trait ProxyExt {
        /// One property, in this connection's own types.
        fn property<T>(&self, name: &str) -> zbus::Result<T>
        where
            T: serde::de::DeserializeOwned + zbus::Type;

        /// Set one property.
        fn put_property<T>(&self, name: &str, value: T) -> zbus::fdo::Result<()>
        where
            T: serde::Serialize + zbus::Type;

        /// Call one method and ignore the reply.
        fn notify<B>(&self, method: &str, body: &B) -> zbus::Result<()>
        where
            B: serde::ser::Serialize + zbus::DynamicType;

        /// Call one method and read the reply.
        fn method<B, R>(&self, method: &str, body: &B) -> zbus::Result<R>
        where
            B: serde::ser::Serialize + zbus::DynamicType,
            R: serde::de::DeserializeOwned + zbus::Type;
    }

    impl ProxyExt for zbus::Proxy<'_> {
        fn property<T>(&self, name: &str) -> zbus::Result<T>
        where
            T: serde::de::DeserializeOwned + zbus::Type,
        {
            block_on(zbus::Proxy::get_property(self, name))
        }

        fn put_property<T>(&self, name: &str, value: T) -> zbus::fdo::Result<()>
        where
            T: serde::Serialize + zbus::Type,
        {
            block_on(zbus::Proxy::set_property(self, name, value))
        }

        fn notify<B>(&self, method: &str, body: &B) -> zbus::Result<()>
        where
            B: serde::ser::Serialize + zbus::DynamicType,
        {
            block_on(zbus::Proxy::call_noreply(self, method, body))
        }

        fn method<B, R>(&self, method: &str, body: &B) -> zbus::Result<R>
        where
            B: serde::ser::Serialize + zbus::DynamicType,
            R: serde::de::DeserializeOwned + zbus::Type,
        {
            block_on(zbus::Proxy::call(self, method, body))
        }
    }

    /// The `fdo` proxies this compositor uses, synchronously.
    ///
    /// zbus 6 generates them async only. `DBusProxy` is the one every worker
    /// reaches for — to see who owns a name, and to hear when that changes — so
    /// it is handled here once rather than in each of them.
    /// A proxy to the bus's own `org.freedesktop.DBus`, synchronously.
    ///
    /// The blocking API built these with `DBusProxy::new`; that is async now,
    /// and this is the same call driven on the calling thread.
    pub fn bus_proxy(connection: &zbus::Connection) -> zbus::Result<zbus::fdo::DBusProxy<'static>> {
        block_on(zbus::fdo::DBusProxy::new(connection))
    }

    pub trait DBusProxyExt {
        /// The unique name of whoever owns `name` right now.
        fn get_name_owner(
            &self,
            name: zbus::names::BusName<'_>,
        ) -> Result<zbus::names::OwnedUniqueName, zbus::fdo::Error>;

        /// Every name currently owned on the bus.
        fn list_names(&self) -> Result<Vec<zbus::names::OwnedBusName>, zbus::fdo::Error>;
    }

    impl DBusProxyExt for zbus::fdo::DBusProxy<'static> {
        fn get_name_owner(
            &self,
            name: zbus::names::BusName<'_>,
        ) -> Result<zbus::names::OwnedUniqueName, zbus::fdo::Error> {
            block_on(zbus::fdo::DBusProxy::get_name_owner(self, name))
        }

        fn list_names(&self) -> Result<Vec<zbus::names::OwnedBusName>, zbus::fdo::Error> {
            block_on(zbus::fdo::DBusProxy::list_names(self))
        }
    }

    /// `ObjectServer`, synchronously.
    ///
    /// Publishing an interface and taking one off the bus are async in zbus 6;
    /// the workers do both from their own threads, at a point where blocking on
    /// the round trip is what the blocking API did too.
    pub trait ObjectServerExt {
        /// Serve `iface` at `path`.
        fn at_blocking<I>(&self, path: zbus::OwnedObjectPath, iface: I) -> zbus::Result<Option<I>>
        where
            I: zbus::object_server::Interface;

        /// Stop serving `I` at `path`.
        fn remove_blocking<I>(&self, path: zbus::OwnedObjectPath) -> zbus::Result<bool>
        where
            I: zbus::object_server::Interface;
    }

    impl ObjectServerExt for zbus::ObjectServer {
        fn at_blocking<I>(&self, path: zbus::OwnedObjectPath, iface: I) -> zbus::Result<Option<I>>
        where
            I: zbus::object_server::Interface,
        {
            block_on(zbus::ObjectServer::at(self, path, iface))
        }

        fn remove_blocking<I>(&self, path: zbus::OwnedObjectPath) -> zbus::Result<bool>
        where
            I: zbus::object_server::Interface,
        {
            block_on(zbus::ObjectServer::remove::<I, zbus::OwnedObjectPath>(
                self, path,
            ))
        }
    }

    /// Drive one blocking `next` of a zbus signal stream.
    ///
    /// zbus 6's `SignalStream` is a `futures_core::Stream`, and the blocking
    /// `SignalStream` that used to be a plain iterator is gone. The two
    /// watchers are loops over the typed signal, so this is how each turn of
    /// those loops waits: block the calling thread until the bus says
    /// something.
    pub fn next_signal<S, T>(stream: &mut S) -> Option<T>
    where
        S: futures_core::Stream<Item = T> + Unpin,
    {
        block_on(std::future::poll_fn(|cx| {
            std::pin::Pin::new(&mut *stream).poll_next(cx)
        }))
    }

    /// Follow one match rule, yielding messages.
    ///
    /// The blocking `MessageIterator` was this plus a cursor; [`next`] is the
    /// `for message in messages` loop it replaced, blocking the calling thread
    /// until a message arrives, which is what the signal threads do.
    ///
    /// [`next`]: MessageIterator::next
    pub struct MessageIterator {
        stream: std::pin::Pin<Box<MessageStream>>,
    }

    impl MessageIterator {
        /// Follow `rule` on `connection`, starting from now.
        ///
        /// Synchronous because the blocking iterator it replaces was: the
        /// signal threads set one up before they start looping, and a thread
        /// that has to `await` a subscription cannot be written as a loop over
        /// `next`.
        pub fn for_match_rule(
            rule: zbus::MatchRule<'static>,
            connection: &zbus::Connection,
            _last_seen: Option<Message>,
        ) -> zbus::Result<Self> {
            let stream = block_on(MessageStream::for_match_rule(rule, connection, None))?;
            Ok(Self {
                stream: Box::pin(stream),
            })
        }

        /// The next message on the rule, or `None` once the connection is gone.
        ///
        /// Blocks the calling thread, as the iterator it replaces did.
        fn advance(&mut self) -> Option<zbus::Result<Message>> {
            use ordered_stream::OrderedStreamExt as _;
            block_on(self.stream.next())
        }
    }

    impl Iterator for MessageIterator {
        type Item = zbus::Result<Message>;

        fn next(&mut self) -> Option<Self::Item> {
            self.advance()
        }
    }
}

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
pub fn name_flags() -> zbus::fdo::RequestNameFlags {
    zbus::fdo::RequestNameFlags::ALLOW_REPLACEMENT
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
    use zbus::fdo::RequestNameFlags as Flags;

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
            !flags.contains(Flags::DO_NOT_QUEUE),
            "being replaced once would mean never holding the name again"
        );
        assert!(
            !flags.contains(Flags::REPLACE_EXISTING),
            "whoever holds it now knows more about this session than we do"
        );
        assert!(
            flags.contains(Flags::ALLOW_REPLACEMENT),
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
