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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Mutex};

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

// Byte ceilings on what a player may put on the bar. MPRIS metadata is D-Bus
// from any application on the session bus, and the shell repaints the whole
// widget from it: a track title is displayed, not stored, but an unbounded one
// still crosses the control socket, enters the shell's queue and allocates in
// a web page. The caps are generous next to real metadata.
const MAX_TITLE: usize = 512;
const MAX_ARTIST: usize = 512;
const MAX_ALBUM: usize = 512;
const MAX_ART: usize = 2048;
/// A data: URL is already-encoded art, not a URL to bound: truncating it
/// produces invalid base64, which is a broken image rather than a smaller
/// one. `icon::art_data_url` has already capped the source file; this is only
/// the outer bound on the string one player can publish.
const MAX_ART_DATA: usize = 16 << 20;

/// A UTF-8-safe prefix of `text` no longer than `max` bytes.
fn truncate(mut text: String, max: usize, property: &str) -> String {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    tracing::debug!(
        "mpris {property} truncated from {} to {end} bytes",
        text.len()
    );
    text.truncate(end);
    text
}

/// Bound cover art without corrupting it: a data URL that is too large is
/// dropped whole, a remote URL is truncated like any other metadata string.
fn bound_art(url: String) -> String {
    let cap = if url.starts_with("data:") {
        MAX_ART_DATA
    } else {
        MAX_ART
    };
    if url.len() <= cap {
        return url;
    }
    tracing::debug!(
        "mpris art is {} bytes, over the {cap} byte cap; dropped",
        url.len()
    );
    String::new()
}

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
    /// One or more bus names appeared, were taken again, or went away since
    /// the last look. This is the one event that says a player marked
    /// unresponsive deserves another chance, so the names themselves are in
    /// [`MPRIS_OWNERS_CHANGED`] rather than in the command.
    OwnerChanged,
    Control(String),
    Enable(bool),
}

/// Whether an MPRIS refresh is already queued.
///
/// The signal rule matches any client's `PropertiesChanged` at the MPRIS
/// path, so a flood would otherwise enqueue one `Refresh` — a `ListNames` and
/// a round trip per player — per message, faster than the worker drains them.
/// `dbus_util::pump` takes a plain `fn`, so this is a static rather than a
/// captured flag; there is one MPRIS worker per process.
static MPRIS_REFRESH_PENDING: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Note that something changed, coalescing a burst into one queued refresh.
///
/// The bit is the only queue here: a player that reports its position every
/// second would otherwise put a `Refresh` behind every message, and the worker
/// would spend its life re-reading a player that has not changed.
fn note_refresh(commands: &mpsc::Sender<Command>) {
    if !MPRIS_REFRESH_PENDING.swap(true, Ordering::AcqRel) {
        let _ = commands.send(Command::Refresh);
    }
}

/// How many bus names one owner-change burst may carry into the worker.
///
/// The names are only needed to clear `unresponsive`; a peer that switches a
/// well-known name on and off in a loop must not grow this list, and the
/// refresh that follows the first name does not need the four-hundredth.
const MAX_OWNER_CHANGES: usize = 256;

/// The most players one refresh probes.
///
/// `ListNames` is a value from the bus daemon, and every name beginning with
/// the MPRIS prefix costs a `PlaybackStatus` round trip — up to the player
/// timeout each for one that has stopped answering. A session with hundreds of
/// names is not a session whose bar can draw hundreds of widgets anyway, so a
/// bounded prefix is the right sample rather than an arbitrary one.
const MAX_PLAYERS_PROBED: usize = 32;

/// Names whose owner changed since the worker last looked, coalesced.
///
/// A `NameOwnerChanged` for an MPRIS name is one `Announce` or `Gone` in the
/// old shape, and both did the same two things: clear the name from
/// `unresponsive` and refresh. One bus peer can flip names in a loop, so the
/// worker gets one command per burst and takes the names from here.
static MPRIS_OWNERS_CHANGED: Mutex<Vec<String>> = Mutex::new(Vec::new());
static MPRIS_OWNERS_PENDING: AtomicBool = AtomicBool::new(false);

/// Note one bus-name transition, coalescing a burst into a single command.
fn note_owner_changed(name: String, commands: &mpsc::Sender<Command>) {
    if let Ok(mut changed) = MPRIS_OWNERS_CHANGED.lock() {
        if changed.len() < MAX_OWNER_CHANGES && !changed.iter().any(|seen| seen == &name) {
            changed.push(name);
        }
    }
    if !MPRIS_OWNERS_PENDING.swap(true, Ordering::AcqRel) {
        let _ = commands.send(Command::OwnerChanged);
    }
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
                |_, commands| note_refresh(commands),
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
                    let Ok((name, _old, _new)) =
                        message.body().deserialize::<(String, String, String)>()
                    else {
                        return;
                    };
                    if !name.starts_with(PREFIX) {
                        return;
                    }
                    // An empty new owner is the name being given up, which is
                    // the player dying; a new one is it announcing itself.
                    // Both clear `unresponsive` and refresh, so one coalesced
                    // command carries either.
                    note_owner_changed(name, commands);
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
                        // A signal that arrived while the widget was off queued
                        // one Refresh and left the coalescing bit set; that
                        // command was then swallowed by the disabled arm below.
                        // Clearing it here is what stops the bit from silencing
                        // every signal for the rest of the session.
                        MPRIS_REFRESH_PENDING.store(false, std::sync::atomic::Ordering::Release);
                        self.refresh();
                    } else {
                        // Nothing on the bar, rather than the last thing that
                        // was playing left behind on it.
                        self.last = None;
                        let _ = self.events.send(Message::Player(None));
                    }
                }
                // Both coalesced commands come before the disabled arm: their
                // bits have to come back down even while the widget is off, or
                // the first burst after it is enabled would be the last one
                // ever seen.
                Command::Refresh => {
                    MPRIS_REFRESH_PENDING.store(false, Ordering::Release);
                    if self.enabled {
                        self.refresh();
                    }
                }
                Command::OwnerChanged => {
                    MPRIS_OWNERS_PENDING.store(false, Ordering::Release);
                    let changed = MPRIS_OWNERS_CHANGED
                        .lock()
                        .map(|mut changed| std::mem::take(&mut *changed))
                        .unwrap_or_default();
                    for name in changed {
                        self.unresponsive.remove(&name);
                    }
                    if self.enabled {
                        // A name that is gone no longer answers `ListNames`, so
                        // the refresh shows whatever is left, or nothing.
                        self.refresh();
                    }
                }
                _ if !self.enabled => {}
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

        // The list is the authority: a name no longer on the bus cannot answer
        // again, so keeping its `unresponsive` row would be memory for
        // nothing. And probing is bounded, because one round trip here can be
        // the player timeout and a hundred fake names would be a hundred of
        // them back to back.
        {
            let listed: HashSet<&str> = names.iter().map(String::as_str).collect();
            self.unresponsive
                .retain(|name| listed.contains(name.as_str()));
        }
        let mut ranked: Vec<(u8, String)> = Vec::new();
        for name in names.into_iter().take(MAX_PLAYERS_PROBED) {
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
        let artist = truncate(
            metadata
                .get("xesam:artist")
                .and_then(|value| <Vec<String>>::try_from(value.clone()).ok())
                .unwrap_or_default()
                .join(", "),
            MAX_ARTIST,
            "artist",
        );
        let title = truncate(text("xesam:title"), MAX_TITLE, "title");

        // A player that is running with nothing loaded — a browser that has
        // published the interface for a tab that has no media yet — has
        // nothing worth a widget.
        if title.is_empty() && artist.is_empty() && status.is_empty() {
            return None;
        }

        Some(MprisPlayer {
            // The bus name without the prefix, which is what a player calls
            // itself: `spotify`, `mpv`, `firefox.instance_1_15`.
            // `strip_prefix`, not `trim_start_matches`: the latter strips every
            // repetition, so a bus name that repeats the prefix yields an id
            // whose controls are addressed to a different player.
            id: name.strip_prefix(PREFIX).unwrap_or(name).to_owned(),
            title,
            artist,
            album: truncate(text("xesam:album"), MAX_ALBUM, "album"),
            status: status.to_lowercase(),
            art: bound_art(art_url(&text("mpris:artUrl"))),
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

    /// A signal that arrived while the widget was off queued a Refresh the
    /// disabled worker swallowed, but left the coalescing bit set. Enabling the
    /// widget clears it, so the next signal is heard; without that the widget
    /// never updates again for the life of the session.
    #[test]
    fn a_refresh_swallowed_while_disabled_does_not_silence_the_next_signal() {
        use std::sync::atomic::Ordering;
        let (commands, inbox) = mpsc::channel();
        MPRIS_REFRESH_PENDING.store(false, Ordering::Release);
        note_refresh(&commands);
        assert!(
            inbox.try_recv().is_ok(),
            "the first signal queues a refresh"
        );
        assert!(
            MPRIS_REFRESH_PENDING.load(Ordering::Acquire),
            "the burst is coalesced"
        );
        note_refresh(&commands);
        assert!(inbox.try_recv().is_err(), "a second signal is coalesced");
        // What `Command::Enable(true)` does before its own refresh.
        MPRIS_REFRESH_PENDING.store(false, Ordering::Release);
        note_refresh(&commands);
        assert!(
            inbox.try_recv().is_ok(),
            "a signal after the widget came back was silenced by a stale pending bit"
        );
    }

    /// A peer flipping an MPRIS name in a loop must not put one command per
    /// message into the worker's channel, and the names have to survive until
    /// the worker takes them: clearing `unresponsive` is why they are carried
    /// at all.
    #[test]
    fn an_owner_change_burst_is_coalesced_into_one_command() {
        let (commands, inbox) = mpsc::channel();
        MPRIS_OWNERS_PENDING.store(false, Ordering::Release);
        MPRIS_OWNERS_CHANGED.lock().unwrap().clear();
        note_owner_changed("org.mpris.MediaPlayer2.a".to_owned(), &commands);
        note_owner_changed("org.mpris.MediaPlayer2.b".to_owned(), &commands);
        assert!(
            matches!(inbox.try_recv(), Ok(Command::OwnerChanged)),
            "the first name queues the one command"
        );
        assert!(
            inbox.try_recv().is_err(),
            "the rest of the burst is coalesced"
        );
        let names = std::mem::take(&mut *MPRIS_OWNERS_CHANGED.lock().unwrap());
        assert!(names.contains(&"org.mpris.MediaPlayer2.b".to_owned()));

        // The worker clears the bit when it takes the command; the next
        // change must then be seen again.
        MPRIS_OWNERS_PENDING.store(false, Ordering::Release);
        note_owner_changed("org.mpris.MediaPlayer2.c".to_owned(), &commands);
        assert!(matches!(inbox.try_recv(), Ok(Command::OwnerChanged)));
    }

    #[test]
    fn metadata_is_capped_without_splitting_a_character() {
        // A two-byte character at the cut point moves the cut one byte left.
        assert_eq!(truncate("aé".to_owned(), 2, "title"), "a");
        assert_eq!(truncate("aé".to_owned(), 3, "title"), "aé");
        assert_eq!(truncate("short".to_owned(), 64, "title"), "short");

        let long = "x".repeat(MAX_TITLE * 3);
        assert_eq!(truncate(long, MAX_TITLE, "title").len(), MAX_TITLE);
    }
}
