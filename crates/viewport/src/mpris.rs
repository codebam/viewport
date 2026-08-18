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

use std::collections::HashMap;
use std::sync::mpsc;

use viewport_ipc::event::MprisPlayer;

/// The bus names this watches for.
const PREFIX: &str = "org.mpris.MediaPlayer2.";
const PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER: &str = "org.mpris.MediaPlayer2.Player";

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
            match start(events) {
                Ok(worker) => self.worker = Some(worker),
                Err(e) => {
                    tracing::warn!("media controls are unavailable: {e:#}");
                    return;
                }
            }
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
    Control(String),
    Enable(bool),
}

fn start(
    events: smithay::reexports::calloop::channel::Sender<Message>,
) -> anyhow::Result<mpsc::Sender<Command>> {
    let (commands, inbox) = mpsc::channel();
    let connection = zbus::blocking::Connection::session()?;

    // Everything a player says about itself comes through one signal, and this
    // takes it for every player at once rather than subscribing per player: a
    // rule per player would mean adding and removing them as players come and
    // go, for a message that is cheap to over-receive.
    pump(
        connection.clone(),
        commands.clone(),
        format!("type='signal',interface='org.freedesktop.DBus.Properties',path='{PATH}'"),
    )?;
    // And players appearing or going away, which is not a property of anything.
    pump(
        connection.clone(),
        commands.clone(),
        "type='signal',sender='org.freedesktop.DBus',\
         interface='org.freedesktop.DBus',member='NameOwnerChanged'"
            .to_owned(),
    )?;

    std::thread::Builder::new()
        .name("mpris".to_owned())
        .spawn(move || Worker::new(connection, events).run(&inbox))?;
    Ok(commands)
}

/// One thread turning a match rule into refreshes.
fn pump(
    connection: zbus::blocking::Connection,
    commands: mpsc::Sender<Command>,
    rule: String,
) -> anyhow::Result<()> {
    let rule = zbus::MatchRule::try_from(rule.as_str())?;
    let messages = zbus::blocking::MessageIterator::for_match_rule(rule, &connection, None)?;
    std::thread::Builder::new()
        .name("mpris-signals".to_owned())
        .spawn(move || {
            for _ in messages.flatten() {
                if commands.send(Command::Refresh).is_err() {
                    return;
                }
            }
        })?;
    Ok(())
}

struct Worker {
    connection: zbus::blocking::Connection,
    events: smithay::reexports::calloop::channel::Sender<Message>,
    /// What was last sent, so an unchanged sample costs the shell nothing. A
    /// player that reports its position through `PropertiesChanged` — several
    /// do, every second — would otherwise redraw the desktop on a timer.
    last: Option<MprisPlayer>,
    enabled: bool,
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
    fn pick(&self) -> Option<String> {
        let names: Vec<String> = zbus::blocking::fdo::DBusProxy::new(&self.connection)
            .ok()?
            .list_names()
            .ok()?
            .into_iter()
            .map(|name| name.as_str().to_owned())
            .filter(|name| name.starts_with(PREFIX))
            .collect();

        names
            .into_iter()
            .map(|name| {
                let rank = match self.status(&name).as_str() {
                    "Playing" => 0,
                    "Paused" => 1,
                    _ => 2,
                };
                (rank, name)
            })
            .min()
            .map(|(_, name)| name)
    }

    fn status(&self, name: &str) -> String {
        self.proxy(name)
            .and_then(|proxy| proxy.get_property::<String>("PlaybackStatus").ok())
            .unwrap_or_default()
    }

    /// Everything the bar draws, from one player.
    fn read(&self, name: &str) -> Option<MprisPlayer> {
        let proxy = self.proxy(name)?;
        let metadata: HashMap<String, zvariant::OwnedValue> =
            proxy.get_property("Metadata").unwrap_or_default();

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

        let status = proxy
            .get_property::<String>("PlaybackStatus")
            .unwrap_or_default();
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
            can_go_next: proxy.get_property("CanGoNext").unwrap_or(false),
            can_go_previous: proxy.get_property("CanGoPrevious").unwrap_or(false),
            // Two properties, and a player answers both — `CanPause` is false
            // for a live stream that can only be stopped, and a button that
            // does nothing is worse than no button.
            can_pause: proxy.get_property("CanPause").unwrap_or(false),
            can_play: proxy.get_property("CanPlay").unwrap_or(false),
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
fn art_url(url: &str) -> String {
    if url.is_empty() || url.starts_with("data:") || url.starts_with("https://") {
        return url.to_owned();
    }
    let Some(path) = url.strip_prefix("file://") else {
        // An unknown scheme is dropped rather than handed on: what would
        // reach the page is an image element that cannot load.
        return String::new();
    };
    crate::icon::data_url(std::path::Path::new(path)).unwrap_or_default()
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
}
