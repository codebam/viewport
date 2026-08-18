// SPDX-License-Identifier: GPL-3.0-or-later
//
// A media player, for testing the bar's media widget without one installed.
//
// What every player on a Linux desktop publishes: a bus name beginning
// `org.mpris.MediaPlayer2.`, an object at `/org/mpris/MediaPlayer2`, and the
// `Player` interface behind it. This one plays nothing at all — it answers
// questions about a track that does not exist and prints the buttons it is
// pressed with, which is the half that cannot be tested in Rust.
//
//   cargo run --example mpris-player -- [title]

use std::collections::HashMap;
use std::io::Write;
use std::sync::Mutex;

use zvariant::Value;

fn say(line: &str) {
    println!("{line}");
    let _ = std::io::stdout().flush();
}

struct Player {
    title: String,
    /// Playing or paused, which `PlayPause` moves between — a test that
    /// presses the button wants to see the state follow.
    playing: Mutex<bool>,
}

#[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
impl Player {
    #[zbus(property)]
    fn playback_status(&self) -> String {
        let playing = *self.playing.lock().expect("the playback state");
        if playing { "Playing" } else { "Paused" }.to_owned()
    }

    /// The metadata map, whose keys are the xesam vocabulary every player
    /// uses. The artist is a list, because a track can have several.
    #[zbus(property)]
    fn metadata(&self) -> HashMap<String, Value<'static>> {
        HashMap::from([
            (
                "mpris:trackid".to_owned(),
                Value::from("/org/viewport/track/1".to_owned()),
            ),
            ("xesam:title".to_owned(), Value::from(self.title.clone())),
            (
                "xesam:artist".to_owned(),
                Value::from(vec!["Aphex Twin".to_owned()]),
            ),
            (
                "xesam:album".to_owned(),
                Value::from("Selected Ambient Works".to_owned()),
            ),
        ])
    }

    #[zbus(property)]
    fn can_go_next(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_go_previous(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn can_play(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_pause(&self) -> bool {
        true
    }

    async fn play_pause(
        &self,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
    ) {
        {
            let mut playing = self.playing.lock().expect("the playback state");
            *playing = !*playing;
        }
        say("play-pause");
        // As a real player does: the host is watching PropertiesChanged rather
        // than polling, so a state change nobody announces is one nothing sees.
        let _ = self.playback_status_changed(&emitter).await;
    }

    fn next(&self) {
        say("next");
    }

    fn previous(&self) {
        say("previous");
    }

    fn stop(&self) {
        say("stop");
    }
}

/// The root interface. A player publishes it beside `Player`, and a host that
/// reads `Identity` needs it to be there.
struct Root;

#[zbus::interface(name = "org.mpris.MediaPlayer2")]
impl Root {
    #[zbus(property)]
    fn identity(&self) -> String {
        "Viewport test player".to_owned()
    }

    #[zbus(property)]
    fn can_quit(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn can_raise(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn has_track_list(&self) -> bool {
        false
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let title = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "Rhubarb".to_owned());

    let connection = zbus::blocking::connection::Builder::session()?
        .serve_at(
            "/org/mpris/MediaPlayer2",
            Player {
                title,
                playing: Mutex::new(true),
            },
        )?
        .serve_at("/org/mpris/MediaPlayer2", Root)?
        .build()?;

    // The name every player takes, with something after the prefix to say
    // which player it is.
    let name = format!("org.mpris.MediaPlayer2.viewporttest{}", std::process::id());
    connection.request_name(name.as_str())?;
    say(&format!("registered {name}"));

    loop {
        std::thread::park();
    }
}
