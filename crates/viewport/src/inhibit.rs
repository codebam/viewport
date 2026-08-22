// SPDX-License-Identifier: GPL-3.0-or-later
//
// Keeping the screen awake for the programs that ask over D-Bus.
//
// `idle-inhibit-v1` is honoured already (see `refresh_idle_inhibit` in
// state.rs), and it is not the interface anything actually uses. A browser
// playing video, a video player, a presentation tool: all of them inhibit over
// the session bus, because that is the interface that existed before Wayland
// and the one every toolkit already had code for. Firefox holds
// `org.freedesktop.ScreenSaver`; anything sandboxed holds
// `org.freedesktop.portal.Inhibit`, which the frontend hands to a backend —
// this one. With neither answered, a film on this desktop is watched with the
// screen blanking under it, and the fix a user finds is to turn the idle
// policy off for everything.
//
// So both are answered here, and both end in the same place: one registry,
// read once a second by the idle timer. What the two interfaces disagree about
// is only how a hold is named — a cookie for the screensaver interface, a
// request object on the bus for the portal — and how it ends.
//
// **A hold has to end when its owner dies.** A player killed mid-film never
// calls `UnInhibit`, and a compositor that waited for one would keep the
// screens lit until the session ended, with nothing on screen to say why. The
// bus says exactly when a connection goes, so every hold records the unique
// name that took it and `watch_owners` drops the lot when that name leaves.
// This is the same reasoning — and the same shape — as
// `screencast::portal::watch_frontend`.
//
// It runs on a connection of its own with a thread behind it, like every other
// bus service here: zbus wants an async runtime and this loop is GLib with
// calloop nested inside it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use zvariant::{ObjectPath, OwnedObjectPath, OwnedValue};

/// The name a screensaver holds, which is what a browser looks for.
const SCREENSAVER_NAME: &str = "org.freedesktop.ScreenSaver";

/// Where the interface is served.
///
/// Both paths, because both are in use. The specification says the first;
/// GNOME has always also answered at the second, so a good deal of software
/// asks there — Firefox tries `/ScreenSaver` when the freedesktop path fails.
/// Serving one object at two paths costs nothing and turns a whole class of
/// "it works on GNOME" into "it works".
const SCREENSAVER_PATHS: [&str; 2] = ["/org/freedesktop/ScreenSaver", "/ScreenSaver"];

/// The portal flag that means "do not let the session go idle".
///
/// The others in the same word are 1 (logging out), 2 (switching user) and 4
/// (suspending). This compositor cannot honour those: it does not own the
/// logout, and the only suspend it performs is the lid's. A hold that names
/// them is kept anyway — see `PortalInhibit::inhibit` for why refusing would
/// be worse.
const INHIBIT_IDLE: u32 = 8;

/// What the bus thread tells the compositor.
///
/// Only the one thing, because the registry itself is shared: the idle timer
/// reads it directly once a second (`refresh_idle_inhibit`), so a hold taken
/// or released needs no message at all. What cannot be read out of a table is
/// somebody saying they are there.
#[derive(Debug)]
pub enum Message {
    /// `SimulateUserActivity`: treat this as though somebody had touched the
    /// machine.
    Activity,
}

/// One hold, by whoever took it.
#[derive(Debug, Clone)]
struct Held {
    /// The unique bus name that asked. What a hold is released against, and
    /// what it dies with.
    owner: Option<String>,
    /// What the application called itself, for the log — a screen that will
    /// not blank is a question, and the answer is a name.
    app: String,
    reason: String,
}

#[derive(Debug, Default)]
struct Inner {
    /// The last cookie handed out. Cookies are never reused within a session.
    next: u32,
    /// Holds taken over `org.freedesktop.ScreenSaver`, by cookie.
    cookies: HashMap<u32, Held>,
    /// Holds taken through the portal, by the request handle that ends them.
    requests: HashMap<OwnedObjectPath, Held>,
    /// The portal connection, once there is one, so a request abandoned by a
    /// dead frontend can be taken off the bus as well as out of this table.
    portal: Option<zbus::blocking::Connection>,
}

/// Everything holding idle off, shared between the two interfaces.
#[derive(Clone, Default)]
pub struct Registry(Arc<Mutex<Inner>>);

impl Registry {
    /// Whether anything at all is holding idle off.
    pub fn inhibited(&self) -> bool {
        let inner = self.0.lock().unwrap();
        !inner.cookies.is_empty() || !inner.requests.is_empty()
    }

    /// Take a hold and name it with a cookie.
    fn hold(&self, owner: Option<String>, app: &str, reason: &str) -> u32 {
        let mut inner = self.0.lock().unwrap();
        // Zero is the value a client that never got one has, so cookies start
        // at one and a zero handed back is always this compositor's fault
        // rather than an ambiguity.
        inner.next = inner.next.wrapping_add(1).max(1);
        let cookie = inner.next;
        inner.cookies.insert(
            cookie,
            Held {
                owner,
                app: app.to_owned(),
                reason: reason.to_owned(),
            },
        );
        cookie
    }

    /// Release a hold, if the caller is the one that took it.
    ///
    /// Checked rather than trusted: a cookie is a small integer on a bus every
    /// process in the session can reach, and one program guessing another's
    /// would turn the screen off in the middle of somebody's film.
    fn release(&self, cookie: u32, owner: Option<&str>) -> Option<Held> {
        let mut inner = self.0.lock().unwrap();
        let held = inner.cookies.get(&cookie)?;
        if held.owner.as_deref() != owner {
            tracing::warn!(
                "inhibit: {owner:?} tried to release cookie {cookie}, which belongs to {:?}",
                held.owner
            );
            return None;
        }
        inner.cookies.remove(&cookie)
    }

    /// Take a hold named by the portal request that will end it.
    fn hold_request(&self, path: OwnedObjectPath, owner: Option<String>, app: &str, reason: &str) {
        self.0.lock().unwrap().requests.insert(
            path,
            Held {
                owner,
                app: app.to_owned(),
                reason: reason.to_owned(),
            },
        );
    }

    fn release_request(&self, path: &OwnedObjectPath) -> Option<Held> {
        self.0.lock().unwrap().requests.remove(path)
    }

    /// Drop everything a departed bus name was holding.
    ///
    /// Returns the request handles that went with it, because those are
    /// objects on the bus as well as rows in a table and both have to go.
    fn drop_owner(&self, name: &str) -> Vec<OwnedObjectPath> {
        let mut inner = self.0.lock().unwrap();
        let owner = Some(name);
        let cookies: Vec<u32> = inner
            .cookies
            .iter()
            .filter(|(_, held)| held.owner.as_deref() == owner)
            .map(|(cookie, _)| *cookie)
            .collect();
        for cookie in &cookies {
            if let Some(held) = inner.cookies.remove(cookie) {
                tracing::info!(
                    "inhibit: {} went away; releasing its hold ({})",
                    held.app,
                    held.reason
                );
            }
        }
        let paths: Vec<OwnedObjectPath> = inner
            .requests
            .iter()
            .filter(|(_, held)| held.owner.as_deref() == owner)
            .map(|(path, _)| path.clone())
            .collect();
        for path in &paths {
            if let Some(held) = inner.requests.remove(path) {
                tracing::info!(
                    "inhibit: {} went away; releasing its request ({})",
                    held.app,
                    held.reason
                );
            }
        }
        paths
    }

    /// Remember the portal connection, so abandoned requests can be removed
    /// from it. Set once the portal object is actually on the bus.
    pub fn set_portal_connection(&self, connection: zbus::blocking::Connection) {
        self.0.lock().unwrap().portal = Some(connection);
    }

    fn portal_connection(&self) -> Option<zbus::blocking::Connection> {
        self.0.lock().unwrap().portal.clone()
    }

    #[cfg(test)]
    fn holds(&self) -> usize {
        let inner = self.0.lock().unwrap();
        inner.cookies.len() + inner.requests.len()
    }
}

/// `org.freedesktop.ScreenSaver`, the interface a browser reaches for.
pub struct ScreenSaver {
    registry: Registry,
    sender: smithay::reexports::calloop::channel::Sender<Message>,
}

#[zbus::interface(name = "org.freedesktop.ScreenSaver")]
impl ScreenSaver {
    /// Hold the screensaver off, and answer with the cookie that releases it.
    fn inhibit(
        &self,
        application_name: &str,
        reason_for_inhibit: &str,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> u32 {
        let owner = header.sender().map(|name| name.to_string());
        let cookie = self
            .registry
            .hold(owner, application_name, reason_for_inhibit);
        tracing::info!(
            "inhibit: {application_name} is holding the screen awake ({reason_for_inhibit})"
        );
        cookie
    }

    /// Give a hold back.
    ///
    /// A cookie nobody holds is not an error the caller can act on — the state
    /// it wanted is the state it has — so this says nothing back and logs.
    fn un_inhibit(&self, cookie: u32, #[zbus(header)] header: zbus::message::Header<'_>) {
        let owner = header.sender().map(|name| name.to_string());
        match self.registry.release(cookie, owner.as_deref()) {
            Some(held) => tracing::info!("inhibit: {} released the screen", held.app),
            None => tracing::debug!("inhibit: nothing holds cookie {cookie}"),
        }
    }

    /// Whether a screensaver is drawing right now.
    ///
    /// Always false, and honestly so. A screensaver is a thing that comes up
    /// on its own and goes away when you touch the mouse, and this compositor
    /// has none of those: locking is either `lock_command`, an
    /// `ext-session-lock` client in a process of its own, or the shell's own
    /// lock screen — and a lock screen is not a screensaver, because touching
    /// the mouse does not end it. Answering the call still matters, because a
    /// client that gets an error on it sometimes concludes the whole interface
    /// is missing and stops inhibiting too.
    fn get_active(&self) -> bool {
        false
    }

    /// How long that screensaver has been up, which for the same reason is
    /// none.
    fn get_active_time(&self) -> u32 {
        0
    }

    /// Somebody is there, even though no input device saw it.
    ///
    /// This is how a program that knows better than the input layer — a
    /// presentation advancing itself, a remote session — pushes both deadlines
    /// back. It goes through the same path a keypress does, so a blanked
    /// screen comes back on.
    fn simulate_user_activity(&self) {
        let _ = self.sender.send(Message::Activity);
    }
}

/// `org.freedesktop.impl.portal.Inhibit`, for anything speaking to the portal.
///
/// Served on the portal connection beside Settings, ScreenCast, RemoteDesktop
/// and Screenshot — they share a bus name, and a second connection asking for
/// it does not get it.
pub struct PortalInhibit {
    registry: Registry,
}

impl PortalInhibit {
    pub fn new(registry: Registry) -> Self {
        Self { registry }
    }
}

#[zbus::interface(name = "org.freedesktop.impl.portal.Inhibit")]
impl PortalInhibit {
    /// Version one deliberately.
    ///
    /// Two adds `CreateMonitor` and `QueryEndResponse`, which are about the
    /// session ending — the frontend asking the backend to put a "you are
    /// about to be logged out" dialog on screen and wait for an answer. There
    /// is no logout here to be about: quitting is `exit`, and the compositor
    /// is the session. Claiming the version and then answering those two with
    /// nothing would leave a frontend waiting on a dialog nobody is drawing,
    /// so the version says what is implemented.
    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        1
    }

    /// Hold something off for as long as the request object lives.
    ///
    /// The frontend gives the handle the application will close; this exports
    /// a `Request` there, and closing it is what releases the hold. That is
    /// the portal's own lifetime rule and it is a better one than a cookie: an
    /// application that crashes has its request closed by the frontend, and a
    /// frontend that crashes is caught by `watch_owners`.
    ///
    /// A hold that names only flags this compositor cannot honour — logout,
    /// switch-user, suspend — is still taken rather than refused. It costs a
    /// screen that stays lit while an application that asked for something
    /// else is running, and refusing costs an application concluding there is
    /// no inhibit backend at all and giving up on the idle flag with it.
    async fn inhibit(
        &self,
        handle: ObjectPath<'_>,
        app_id: &str,
        _window: &str,
        options: HashMap<String, OwnedValue>,
        #[zbus(object_server)] server: &zbus::ObjectServer,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) {
        let flags = options
            .get("flags")
            .and_then(|value| u32::try_from(value).ok())
            // Absent means the frontend did not pass them on. Idle is the
            // reading that keeps a screen lit that should have been, which is
            // the failure the caller can see; the other way round is a film
            // blanking and no way to tell why.
            .unwrap_or(INHIBIT_IDLE);
        let reason = options
            .get("reason")
            .and_then(|value| <&str>::try_from(value).ok().map(str::to_owned))
            .unwrap_or_default();
        let path = OwnedObjectPath::from(handle);
        let owner = header.sender().map(|name| name.to_string());

        if flags & INHIBIT_IDLE == 0 {
            tracing::info!(
                "inhibit: {app_id} asked for flags {flags} ({reason}); \
                 only idle is honoured here"
            );
        } else {
            tracing::info!("inhibit: {app_id} is holding the screen awake ({reason})");
        }

        self.registry
            .hold_request(path.clone(), owner, app_id, &reason);
        let request = RequestObject {
            path: path.clone(),
            registry: self.registry.clone(),
        };
        if let Err(e) = server.at(&path, request).await {
            // The hold stands, but nothing can end it by hand — so end it
            // here rather than leaving a screen lit for the session.
            tracing::warn!("inhibit: could not publish request {path}: {e}");
            self.registry.release_request(&path);
        }
    }
}

/// The request object the frontend closes when the application is done.
struct RequestObject {
    path: OwnedObjectPath,
    registry: Registry,
}

#[zbus::interface(name = "org.freedesktop.impl.portal.Request")]
impl RequestObject {
    async fn close(&self, #[zbus(object_server)] server: &zbus::ObjectServer) {
        match self.registry.release_request(&self.path) {
            Some(held) => tracing::info!("inhibit: {} released the screen", held.app),
            None => tracing::debug!("inhibit: request {} was already closed", self.path),
        }
        // And off the bus, or a desktop accumulates one dead request object
        // per video watched.
        if let Err(e) = server.remove::<RequestObject, _>(&self.path).await {
            tracing::warn!(
                "inhibit: could not take request {} off the bus: {e}",
                self.path
            );
        }
    }
}

/// Claim `org.freedesktop.ScreenSaver` and start answering.
///
/// A failure is not fatal, on the same terms as every other bus service here:
/// a session with no D-Bus, or one where a real screensaver daemon already
/// holds the name, still has a working compositor. It simply has the idle
/// policy it had a moment ago.
pub fn start(
    registry: Registry,
    sender: smithay::reexports::calloop::channel::Sender<Message>,
) -> anyhow::Result<zbus::blocking::Connection> {
    let mut builder = zbus::blocking::connection::Builder::session()?;
    for path in SCREENSAVER_PATHS {
        builder = builder.serve_at(
            path,
            ScreenSaver {
                registry: registry.clone(),
                sender: sender.clone(),
            },
        )?;
    }
    let connection = builder.build()?;

    // With `crate::dbus::name_flags`, like every other name here: queued for
    // rather than taken, so a screensaver daemon somebody started on purpose
    // wins and this gets the name back when that daemon exits.
    let reply = connection.request_name_with_flags(SCREENSAVER_NAME, crate::dbus::name_flags());
    crate::dbus::log_name_reply(SCREENSAVER_NAME, reply);

    watch_owners(connection.clone(), registry);
    Ok(connection)
}

/// Drop every hold a departed connection was holding.
///
/// The thread that makes an inhibit safe to grant. Without it the rule would
/// have to be a timeout — "no application holds the screen awake for more than
/// an hour" — which is wrong in both directions: it cuts off a long film and
/// it still leaves an hour of lit screen after a crash.
fn watch_owners(connection: zbus::blocking::Connection, registry: Registry) {
    std::thread::Builder::new()
        .name("viewport-inhibit-watch".to_owned())
        .spawn(move || {
            let proxy = match zbus::blocking::fdo::DBusProxy::new(&connection) {
                Ok(proxy) => proxy,
                Err(e) => {
                    tracing::warn!("inhibit: could not watch for departures: {e}");
                    return;
                }
            };
            let changes = match proxy.receive_name_owner_changed() {
                Ok(changes) => changes,
                Err(e) => {
                    tracing::warn!("inhibit: could not watch for departures: {e}");
                    return;
                }
            };

            for change in changes {
                let Ok(args) = change.args() else { continue };
                // A name that moved to a new owner is a service restarting,
                // not a client that went away.
                if args.new_owner().is_some() {
                    continue;
                }
                let gone = args.name().to_string();
                let abandoned = registry.drop_owner(&gone);
                if abandoned.is_empty() && !gone.starts_with(':') {
                    // Well-known names come and go all session; only unique
                    // names hold anything here.
                    continue;
                }
                if let Some(portal) = registry.portal_connection() {
                    for path in &abandoned {
                        if let Err(e) = portal.object_server().remove::<RequestObject, _>(path) {
                            tracing::warn!("inhibit: could not take {path} off the bus: {e}");
                        }
                    }
                }
            }
        })
        .map(|_| ())
        .unwrap_or_else(|e| tracing::warn!("inhibit: could not start the watcher: {e}"));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> Registry {
        Registry::default()
    }

    #[test]
    fn nothing_is_held_to_begin_with() {
        assert!(!registry().inhibited());
    }

    #[test]
    fn a_hold_keeps_the_screen_awake_until_it_is_given_back() {
        let registry = registry();
        let cookie = registry.hold(Some(":1.7".to_owned()), "firefox", "playing video");
        assert!(registry.inhibited());
        assert!(registry.release(cookie, Some(":1.7")).is_some());
        assert!(!registry.inhibited());
    }

    #[test]
    fn cookies_are_never_zero() {
        // Zero is what a client that never got one has, so it can never be a
        // cookie: a zero handed back is this compositor's fault rather than an
        // ambiguity the client has to resolve.
        let registry = registry();
        for _ in 0..4 {
            assert_ne!(registry.hold(None, "app", ""), 0);
        }
    }

    #[test]
    fn one_program_cannot_release_anothers_hold() {
        // A cookie is a small integer on a bus every process in the session
        // can reach. Guessing one must not turn somebody else's screen off.
        let registry = registry();
        let cookie = registry.hold(Some(":1.7".to_owned()), "mpv", "playing video");
        assert!(registry.release(cookie, Some(":1.9")).is_none());
        assert!(registry.inhibited(), "the hold stands");
        assert!(registry.release(cookie, Some(":1.7")).is_some());
    }

    #[test]
    fn releasing_a_cookie_nobody_holds_is_not_a_crash() {
        let registry = registry();
        assert!(registry.release(17, Some(":1.7")).is_none());
        assert!(!registry.inhibited());
    }

    #[test]
    fn a_hold_dies_with_the_connection_that_took_it() {
        // The case this exists for: a player killed mid-film never calls
        // UnInhibit, and nothing else would ever release it.
        let registry = registry();
        registry.hold(Some(":1.7".to_owned()), "mpv", "playing video");
        registry.hold(Some(":1.9".to_owned()), "firefox", "playing video");
        assert_eq!(
            registry.drop_owner(":1.7").len(),
            0,
            "no requests, only a cookie"
        );
        assert!(registry.inhibited(), "firefox is still watching something");
        registry.drop_owner(":1.9");
        assert!(!registry.inhibited());
    }

    #[test]
    fn a_portal_request_is_held_until_it_is_closed() {
        let registry = registry();
        let path = OwnedObjectPath::try_from("/org/freedesktop/portal/desktop/request/1_7/t")
            .expect("a valid path");
        registry.hold_request(
            path.clone(),
            Some(":1.7".to_owned()),
            "org.gnome.Totem",
            "video",
        );
        assert!(registry.inhibited());
        assert!(registry.release_request(&path).is_some());
        assert!(!registry.inhibited());
    }

    #[test]
    fn a_dead_frontend_takes_its_requests_with_it() {
        // A frontend that crashed calls Close on nothing, and the request
        // objects it left behind have to come off the bus as well as out of
        // the table — which is why this returns the paths.
        let registry = registry();
        let path = OwnedObjectPath::try_from("/org/freedesktop/portal/desktop/request/1_7/t")
            .expect("a valid path");
        registry.hold_request(
            path.clone(),
            Some(":1.7".to_owned()),
            "org.gnome.Totem",
            "video",
        );
        assert_eq!(registry.drop_owner(":1.7"), vec![path]);
        assert!(!registry.inhibited());
    }

    #[test]
    fn the_two_interfaces_share_one_answer() {
        // Held by a cookie and by a request at once — a browser inhibiting
        // directly while a sandboxed player goes through the portal — and idle
        // stays off until both are gone.
        let registry = registry();
        let cookie = registry.hold(Some(":1.7".to_owned()), "firefox", "playing video");
        let path = OwnedObjectPath::try_from("/org/freedesktop/portal/desktop/request/1_9/t")
            .expect("a valid path");
        registry.hold_request(
            path.clone(),
            Some(":1.9".to_owned()),
            "org.gnome.Totem",
            "video",
        );
        assert_eq!(registry.holds(), 2);

        registry.release(cookie, Some(":1.7"));
        assert!(registry.inhibited(), "the portal hold is still there");
        registry.release_request(&path);
        assert!(!registry.inhibited());
    }
}
