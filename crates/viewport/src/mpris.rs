// SPDX-License-Identifier: GPL-3.0-or-later
//
// What is playing, for the bar.
//
// MPRIS is the one thing every media player on a Linux desktop agrees on: a
// bus name beginning `org.mpris.MediaPlayer2.`, an object at
// `/org/mpris/MediaPlayer2`, and metadata behind it. mpv, Spotify, Firefox and
// every music player publish one, which is why `playerctl` works everywhere
// and why the bar can show a track without knowing what is playing it.
//
// The compositor reads it rather than the shell, for the reason the shell
// reads nothing else either: the page has no bus, and a widget that shelled
// out to `playerctl` twice a second would be two processes a second on an idle
// desktop.
//
// A thread of its own with a channel back, like notifications and the tray.
// Media players are ordinary applications and some of them stop answering
// while they buffer; a compositor that waited on one would drop frames for a
// track title.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc;

use viewport_ipc::event::MprisPlayer;

/// The bus names this watches for.
const PREFIX: &str = "org.mpris.MediaPlayer2.";
const PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER: &str = "org.mpris.MediaPlayer2.Player";

/// How long one player gets to answer before the worker stops waiting.
///
/// Longer than any honest player needs and short enough that a wedged one is
/// a hiccup rather than a hang: media players are ordinary applications and
/// some of them stop answering while they buffer, and a worker that can be
/// parked for minutes by one of them is every click and refresh on the bar
/// parked with it. The same discipline as the tray's `ITEM_TIMEOUT`.
const PLAYER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(4);

/// How long any single call on the connection may run.
///
/// This is what collects the threads [`crate::dbus_util::with_deadline`]
/// walks away from: the worker gives up at `PLAYER_TIMEOUT`, but the thread it
/// handed the proxy to keeps trying until zbus itself gives up. Generous next
/// to `PLAYER_TIMEOUT` on purpose — the deadline that matters is the worker's,
/// and this one must never be what an honest player trips over.
const CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// What the thread sends the compositor: which player the bar should show, or
/// nothing when none is running.
#[derive(Debug)]
pub enum Message {
    Player(Option<MprisPlayer>),
}

/// The half the compositor keeps.
#[derive(Default)]
pub struct Mpris {
    worker: Option<mpsc::Sender<Command>>,
    enabled: bool,
    events: Option<smithay::reexports::calloop::channel::Sender<Message>>,
}

impl Mpris {
    /// Where updates go. Called once, when the event loop has a source.
    pub fn attach(&mut self, events: smithay::reexports::calloop::channel::Sender<Message>) {
        self.events = Some(events);
        let enabled = self.enabled;
        self.enabled = false;
        self.set_enabled(enabled);
    }

    /// Whether anything on the bar wants this.
    ///
    /// Off is the default and costs nothing: no connection, no thread, no
    /// match rules. A desktop with no media widget should not be following
    /// every player on the session, and this is the same rule the status
    /// sampler already applies to `wpctl`.
    pub fn set_enabled(&mut self, enabled: bool) {
        if enabled == self.enabled {
            return;
        }
        self.enabled = enabled;
        let Some(events) = self.events.clone() else {
            return;
        };

        if self.worker.is_none() {
            if !enabled {
                return;
            }
            // Starting no longer touches the bus on this thread — the
            // connection is made inside the worker — so there is nothing to
            // fail here, and a bus that never answers is reported through the
            // events channel like any other empty answer.
            self.worker = Some(start(events));
        }
        self.send(Command::Enable(enabled));
    }

    /// A button on the bar.
    pub fn control(&self, action: String) {
        self.send(Command::Control(action));
    }

    fn send(&self, command: Command) {
        if let Some(worker) = self.worker.as_ref() {
            let _ = worker.send(command);
        }
    }
}

enum Command {
    /// Something about some player changed; ask again. Which player and which
    /// property is not worth tracking: the answer is one round trip either
    /// way, and the bar shows one player.
    Refresh,
    /// A bus name has appeared or been taken again. This is the one event
    /// that says a player marked unresponsive deserves another chance, which
    /// is why it is distinguished from an ordinary refresh.
    Announce(String),
    /// A bus name went away.
    Gone(String),
    Control(String),
    Enable(bool),
}

fn start(events: smithay::reexports::calloop::channel::Sender<Message>) -> mpsc::Sender<Command> {
    let (commands, inbox) = mpsc::channel();

    // Connecting happens here and not on the thread that called `start` —
    // which is the compositor's event loop, on the way out of a configuration
    // reload or the first keystroke in a picker. A round trip to a wedged bus
    // daemon must not stall a frame, and a session with no bus at all is
    // reported through the events channel, the way an empty answer is.
    let (worker_events, worker_commands) = (events.clone(), commands.clone());
    let spawned = std::thread::Builder::new()
        .name("mpris".to_owned())
        .spawn(move || {
            let connection = match zbus::blocking::connection::Builder::session()
                .and_then(|builder| builder.method_timeout(CALL_TIMEOUT).build())
            {
                Ok(connection) => connection,
                Err(e) => {
                    tracing::warn!("media controls are unavailable: {e:#}");
                    let _ = worker_events.send(Message::Player(None));
                    return;
                }
            };

            // Everything a player says about itself comes through one signal,
            // and this takes it for every player at once rather than
            // subscribing per player: a rule per player would mean adding and
            // removing them as players come and go, for a message that is
            // cheap to over-receive.
            if let Err(e) = crate::dbus_util::pump(
                connection.clone(),
                worker_commands.clone(),
                "mpris-signals",
                format!("type='signal',interface='org.freedesktop.DBus.Properties',path='{PATH}'"),
                |_, commands| {
                    let _ = commands.send(Command::Refresh);
                },
            ) {
                tracing::warn!("media controls: could not follow players: {e:#}");
            }
            // And players appearing or going away, which is not a property of
            // anything — and which is also the only notice a wedged player
            // ever gives that it is back.
            if let Err(e) = crate::dbus_util::pump(
                connection.clone(),
                worker_commands.clone(),
                "mpris-signals",
                "type='signal',sender='org.freedesktop.DBus',\
                 interface='org.freedesktop.DBus',member='NameOwnerChanged'"
                    .to_owned(),
                |message, commands| {
                    let Ok((name, _old, new)) =
                        message.body().deserialize::<(String, String, String)>()
                    else {
                        return;
                    };
                    if !name.starts_with(PREFIX) {
                        return;
                    }
                    // An empty new owner is the name being given up, which is
                    // the player dying; a new one is it announcing itself.
                    let command = if new.is_empty() {
                        Command::Gone(name)
                    } else {
                        Command::Announce(name)
                    };
                    let _ = commands.send(command);
                },
            ) {
                tracing::warn!("media controls: could not follow the bus: {e:#}");
            }

            Worker::new(connection, worker_events).run(&inbox);
        });

    if spawned.is_err() {
        tracing::warn!("media controls: the worker could not start");
        let _ = events.send(Message::Player(None));
    }
    commands
}

struct Worker {
    connection: zbus::blocking::Connection,
    events: smithay::reexports::calloop::channel::Sender<Message>,
    /// What was last sent, so an unchanged sample costs the shell nothing. A
    /// player that reports its position through `PropertiesChanged` — several
    /// do, every second — would otherwise redraw the desktop on a timer.
    last: Option<MprisPlayer>,
    enabled: bool,
    /// The players that stopped answering outright, by bus name. A property
    /// that is merely missing is ordinary; a fetch that ran past
    /// [`PLAYER_TIMEOUT`] means the process behind the name is wedged, and
    /// asking it again on every signal would be paying its timeout over and
    /// over — for as long as it keeps sending signals it cannot answer for.
    /// Skipped until it re-announces itself through `Announce`.
    unresponsive: HashSet<String>,
}

impl Worker {
    fn new(
        connection: zbus::blocking::Connection,
        events: smithay::reexports::calloop::channel::Sender<Message>,
    ) -> Self {
        Self {
            connection,
            events,
            last: None,
            enabled: false,
            unresponsive: HashSet::new(),
        }
    }

    fn run(mut self, inbox: &mpsc::Receiver<Command>) {
        while let Ok(command) = inbox.recv() {
            match command {
                Command::Enable(enabled) => {
                    self.enabled = enabled;
                    if enabled {
                        self.refresh();
                    } else {
                        // Nothing on the bar, rather than the last thing that
                        // was playing left behind on it.
                        self.last = None;
                        let _ = self.events.send(Message::Player(None));
                    }
                }
                _ if !self.enabled => {}
                Command::Refresh => self.refresh(),
                Command::Announce(name) => {
                    self.unresponsive.remove(&name);
                    self.refresh();
                }
                Command::Gone(name) => {
                    self.unresponsive.remove(&name);
                    // Not special-cased beyond the bookkeeping: a name that is
                    // gone no longer answers `ListNames`, so the refresh below
                    // shows whatever is left, or nothing.
                    self.refresh();
                }
                Command::Control(action) => self.control(&action),
            }
        }
    }

    /// Which player the bar shows, and what it says.
    fn refresh(&mut self) {
        let player = self.pick().and_then(|name| self.read(&name));
        if player == self.last {
            return;
        }
        self.last = player.clone();
        let _ = self.events.send(Message::Player(player));
    }

    /// The player worth showing.
    ///
    /// One that is playing wins over one that is paused, and a paused one over
    /// one that is stopped — which is the rule `playerctl` uses and the one a
    /// person would apply looking at the screen. Ties go to the first name the
    /// bus lists, which is stable for as long as those players are running.
    /// A player marked unresponsive is not a candidate at all: asking it would
    /// be four seconds of waiting for a known answer.
    fn pick(&mut self) -> Option<String> {
        let listed = crate::dbus_util::with_deadline(PLAYER_TIMEOUT, "mpris-names", {
            let connection = self.connection.clone();
            move || {
                // `list_names` answers in `fdo::Error`; one conversion puts it
                // in the same shape every other bus answer takes here.
                zbus::blocking::fdo::DBusProxy::new(&connection)
                    .and_then(|proxy| proxy.list_names().map_err(zbus::Error::from))
            }
        });
        let Some(Ok(names)) = listed else {
            return None;
        };
        let names: Vec<String> = names
            .into_iter()
            .map(|name| name.as_str().to_owned())
            .filter(|name| name.starts_with(PREFIX))
            .collect();

        let mut ranked: Vec<(u8, String)> = Vec::new();
        for name in names {
            if self.unresponsive.contains(&name) {
                continue;
            }
            let rank = match self.status(&name).as_str() {
                "Playing" => 0,
                "Paused" => 1,
                _ => 2,
            };
            ranked.push((rank, name));
        }
        ranked.into_iter().min().map(|(_, name)| name)
    }

    fn status(&mut self, name: &str) -> String {
        let Some(proxy) = self.proxy(name) else {
            return String::new();
        };
        match crate::dbus_util::with_deadline(PLAYER_TIMEOUT, "mpris-status", move || {
            proxy.get_property::<String>("PlaybackStatus")
        }) {
            Some(Ok(status)) => status,
            Some(Err(_)) => String::new(),
            None => {
                self.stopped_answering(name);
                String::new()
            }
        }
    }

    /// Note that a player has stopped answering, once.
    fn stopped_answering(&mut self, name: &str) {
        if self.unresponsive.insert(name.to_owned()) {
            tracing::warn!(
                "media player {name} stopped answering; skipping it until it re-announces"
            );
        }
    }

    /// Everything the bar draws, from one player.
    ///
    /// Every property read runs under one deadline, on a thread of its own:
    /// this is where a buffering player used to park the whole bar. What comes
    /// back is turned into the widget's shape here, off the stopwatch.
    fn read(&mut self, name: &str) -> Option<MprisPlayer> {
        let proxy = self.proxy(name)?;
        let fetched = crate::dbus_util::with_deadline(PLAYER_TIMEOUT, "mpris-read", move || {
            let metadata: HashMap<String, zvariant::OwnedValue> =
                proxy.get_property("Metadata").unwrap_or_default();
            (
                metadata,
                proxy
                    .get_property::<String>("PlaybackStatus")
                    .unwrap_or_default(),
                proxy.get_property("CanGoNext").unwrap_or(false),
                proxy.get_property("CanGoPrevious").unwrap_or(false),
                proxy.get_property("CanPause").unwrap_or(false),
                proxy.get_property("CanPlay").unwrap_or(false),
            )
        });
        let Some((metadata, status, can_go_next, can_go_previous, can_pause, can_play)) = fetched
        else {
            self.stopped_answering(name);
            return None;
        };

        let text = |key: &str| -> String {
            metadata
                .get(key)
                .and_then(|value| <&str>::try_from(value).ok())
                .unwrap_or_default()
                .to_owned()
        };
        // Artists are a list, because a track can have several, and every
        // player sends one even for the single case.
        let artist = metadata
            .get("xesam:artist")
            .and_then(|value| <Vec<String>>::try_from(value.clone()).ok())
            .unwrap_or_default()
            .join(", ");
        let title = text("xesam:title");

        // A player that is running with nothing loaded — a browser that has
        // published the interface for a tab that has no media yet — has
        // nothing worth a widget.
        if title.is_empty() && artist.is_empty() && status.is_empty() {
            return None;
        }

        Some(MprisPlayer {
            // The bus name without the prefix, which is what a player calls
            // itself: `spotify`, `mpv`, `firefox.instance_1_15`.
            id: name.trim_start_matches(PREFIX).to_owned(),
            title,
            artist,
            album: text("xesam:album"),
            status: status.to_lowercase(),
            art: art_url(&text("mpris:artUrl")),
            can_go_next,
            can_go_previous,
            // Two properties, and a player answers both — `CanPause` is false
            // for a live stream that can only be stopped, and a button that
            // does nothing is worse than no button.
            can_pause,
            can_play,
        })
    }

    /// A button, sent to whichever player the bar is showing.
    ///
    /// Named rather than passed through: this is a string from a page, and the
    /// interface has methods that a bar has no business calling — `OpenUri`
    /// takes a URI and `SetPosition` takes a track.
    fn control(&self, action: &str) {
        let method = match action {
            "play-pause" => "PlayPause",
            "next" => "Next",
            "previous" => "Previous",
            "stop" => "Stop",
            other => {
                tracing::debug!("no such media action {other:?}");
                return;
            }
        };
        let Some(name) = self
            .last
            .as_ref()
            .map(|player| format!("{PREFIX}{}", player.id))
        else {
            return;
        };
        let Some(proxy) = self.proxy(&name) else {
            return;
        };
        if let Err(e) = proxy.call_noreply(method, &()) {
            tracing::debug!("{name}: {method} failed: {e}");
        }
    }

    fn proxy(&self, name: &str) -> Option<zbus::blocking::Proxy<'static>> {
        zbus::blocking::proxy::Builder::new(&self.connection)
            .destination(name.to_owned())
            .ok()?
            .path(PATH)
            .ok()?
            .interface(PLAYER)
            .ok()?
            // The properties change constantly and the signal is what this
            // listens to; a cache would be a second subscription per player.
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .ok()
    }
}

/// Cover art, as something the shell can draw.
///
/// Players send a URL: a `file://` path for a local library, an `https://` one
/// for a streaming service, and occasionally a `data:` URL already. The first
/// is read and encoded here — the shell may be loaded over `http://`, where a
/// `file://` image is refused — and the second is passed through, because the
/// page can fetch it and this compositor has no business making outbound
/// requests on a desktop's behalf.
///
/// A `file://` path is an instruction to open a file, published by whoever
/// could speak on the session bus, and what comes back is base64ed straight
/// into a page that may have been loaded over plain http. So the instruction
/// is taken only within narrow limits: not out of the pseudo-filesystems,
/// and only for names that claim to be pictures — which is also what keeps
/// the legitimate case working, since cover caches live under `~/.cache` and
/// `~/.local/share` and hold files called `folder.jpg`. [`icon::art_data_url`]
/// applies the rest of the discipline: regular files only, and a size cap.
fn art_url(url: &str) -> String {
    if url.is_empty() || url.starts_with("data:") || url.starts_with("https://") {
        return url.to_owned();
    }
    let Some(path) = url.strip_prefix("file://") else {
        // An unknown scheme is dropped rather than handed on: what would
        // reach the page is an image element that cannot load.
        return String::new();
    };
    let path = std::path::Path::new(path);
    if in_pseudo_filesystem(path) {
        tracing::debug!("cover art from {} refused", path.display());
        return String::new();
    }
    crate::icon::art_data_url(path).unwrap_or_default()
}

/// Whether a path lives somewhere the kernel synthesises rather than stores.
///
/// `/proc`, `/sys` and `/dev` hold files whose contents are whatever the
/// kernel makes them — endless zeros, other processes' memory, device
/// streams. Nothing there is a picture, the extension on the name proves
/// nothing about what a read of it returns, and several of them never end.
/// Checked before the extension for exactly that reason: `zero.png` is still
/// `/dev/zero`.
fn in_pseudo_filesystem(path: &std::path::Path) -> bool {
    use std::path::Component;
    matches!(
        path.components().nth(1),
        Some(Component::Normal(name)) if name == "proc" || name == "sys" || name == "dev"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A remote cover is passed through and a local one is read; anything
    /// else is dropped rather than handed to a page that cannot fetch it.
    #[test]
    fn cover_art_is_a_url_the_page_can_actually_draw() {
        assert_eq!(art_url(""), "");
        assert_eq!(art_url("https://cdn/x.jpg"), "https://cdn/x.jpg");
        assert_eq!(
            art_url("data:image/png;base64,AA=="),
            "data:image/png;base64,AA=="
        );
        assert_eq!(art_url("ftp://host/x.png"), "");
        // A file that is not there reads as no art, not as a broken image.
        assert_eq!(art_url("file:///nonexistent/cover.png"), "");
    }

    /// A private key is not a picture, whatever a player claims. The
    /// extension is the whole gate here — which is also why it holds without
    /// a path allowlist: cover caches are full of `folder.jpg` under
    /// `~/.cache`, and nothing anyone legitimately ships as art is called
    /// `id_rsa`.
    #[test]
    fn art_is_not_a_window_onto_the_filesystem() {
        assert_eq!(art_url("file:///home/user/.ssh/id_rsa"), "");
        assert_eq!(art_url("file:///etc/passwd"), "");
    }

    /// The pseudo-filesystems are refused by name, ahead of any other check,
    /// because their files lie about what they are: `/dev/zero` reports no
    /// size at all and never stops being read.
    #[test]
    fn the_pseudo_filesystems_are_refused_by_name() {
        use std::path::Path;
        assert!(in_pseudo_filesystem(Path::new("/proc/self/environ")));
        assert!(in_pseudo_filesystem(Path::new("/sys/class/../zero.png")));
        assert!(in_pseudo_filesystem(Path::new("/dev/shm/cover.png")));
        assert!(!in_pseudo_filesystem(Path::new(
            "/home/user/.cache/covers/folder.jpg"
        )));
        assert!(!in_pseudo_filesystem(Path::new("/protection/cover.png")));
    }

    /// A scratch directory for the two tests that need real files.
    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("viewport-mpris-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        dir
    }

    /// A file over the cap is refused rather than read — this one is sparse,
    /// so the refusal happens on its reported size and nothing reads nine
    /// megabytes of anything.
    #[test]
    fn an_oversized_cover_is_refused_rather_than_read() {
        let dir = scratch("oversized");
        let path = dir.join("cover.png");
        let file = std::fs::File::create(&path).expect("a big empty file");
        file.set_len(crate::icon::MAX_ART + 1).expect("its size");
        drop(file);
        assert_eq!(art_url(&format!("file://{}", path.display())), "");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// And the ordinary case still works: a JPEG in an album folder reaches
    /// the page as something it can draw.
    #[test]
    fn a_local_cover_still_reaches_the_page() {
        let dir = scratch("local");
        let path = dir.join("folder.jpg");
        std::fs::write(&path, b"\xff\xd8\xff\xe0not really a jpeg").expect("a cover");
        let url = art_url(&format!("file://{}", path.display()));
        assert!(
            url.starts_with("data:image/jpeg;base64,"),
            "the cover did not survive: {url}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
