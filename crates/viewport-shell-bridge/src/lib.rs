// SPDX-License-Identifier: MIT
//
// The half of an out-of-process shell that is not the engine.
//
// There are two of these now — WebKitGTK and Chromium — and there will be more.
// What differs between them is how a page is loaded and how a message gets into
// and out of it. What does not differ is everything on this side of that: the
// arguments, the control socket, the framing, and the rule about which
// direction may block which thread. So that lives here, once.
//
// Nothing in this crate knows what an engine is. It hands lines up and takes
// lines down.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use anyhow::{anyhow, Context as _, Result};

/// Everything a shell process needs to be told, and where each of them comes
/// from.
#[derive(Clone, Debug)]
pub struct Options {
    /// The page to load.
    pub url: String,
    /// The compositor's control socket.
    pub socket: PathBuf,
    /// Whether to allow the engine's inspector. For a shell being edited live.
    pub inspector: bool,
}

impl Options {
    /// The command line first, then the environment.
    ///
    /// The compositor sets the environment when it starts the shell itself; the
    /// flags are for running one by hand against a session that is already up,
    /// which is the whole development loop for the shell's own JavaScript.
    pub fn parse(args: &[String]) -> Result<Self> {
        let flag = |name: &str| -> Option<String> {
            let mut it = args.iter();
            while let Some(arg) = it.next() {
                if let Some(rest) = arg.strip_prefix(&format!("{name}=")) {
                    return Some(rest.to_owned());
                }
                if arg == name {
                    return it.next().cloned();
                }
            }
            None
        };

        let url = flag("--url")
            .or_else(|| std::env::var("VIEWPORT_SHELL_URL").ok())
            .ok_or_else(|| {
                anyhow!(
                    "no page to load: pass --url or set VIEWPORT_SHELL_URL. \
                     The compositor sets it when it starts the shell itself"
                )
            })?;

        // The compositor passes the path outright, because a shell started with
        // `WAYLAND_SOCKET` has no `WAYLAND_DISPLAY` to derive it from.
        let socket = match flag("--socket").or_else(|| std::env::var("VIEWPORT_IPC_SOCKET").ok()) {
            Some(path) => PathBuf::from(path),
            None => {
                let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_owned());
                let display = std::env::var("WAYLAND_DISPLAY").map_err(|_| {
                    anyhow!(
                        "no control socket: pass --socket or set VIEWPORT_IPC_SOCKET, \
                         or run under a compositor so WAYLAND_DISPLAY names one"
                    )
                })?;
                PathBuf::from(format!("{dir}/viewport-{display}.sock"))
            }
        };

        Ok(Self {
            url,
            socket,
            inspector: flag("--inspector").is_some()
                || std::env::var("VIEWPORT_SHELL_INSPECTOR").is_ok(),
        })
    }
}

/// What arrives from the compositor.
pub enum Line {
    /// One event, as JSON, exactly as it came off the socket.
    Event(String),
    /// The compositor closed the socket, which means the session is over.
    Closed,
}

/// The page-to-compositor direction.
///
/// Cloneable and `Send`, because the engine callback that produces messages is
/// not on any particular thread and should not have to care.
#[derive(Clone)]
pub struct Sender(mpsc::Sender<String>);

impl Sender {
    /// Queue one already-serialised message.
    ///
    /// Never blocks on the socket. A shell that stopped painting because the
    /// compositor was slow to drain a socket would be a desktop that freezes
    /// exactly when it is most needed.
    pub fn send(&self, json: String) {
        if self.0.send(json).is_err() {
            tracing::error!("the compositor is gone; the message was dropped");
        }
    }
}

/// Connect to the compositor and start both directions.
///
/// `on_line` runs on the reader thread. It is expected to hand the line to
/// whatever loop the engine is running on rather than to act on it — every
/// engine here is single-threaded from the outside.
pub fn connect<F>(path: &Path, mut on_line: F) -> Result<Sender>
where
    F: FnMut(Line) + Send + 'static,
{
    let socket =
        UnixStream::connect(path).with_context(|| format!("connecting to {}", path.display()))?;
    let reader = socket.try_clone().context("duplicating the socket")?;

    let (tx, rx) = mpsc::channel::<String>();
    std::thread::Builder::new()
        .name("ipc-write".into())
        .spawn(move || {
            let mut socket = socket;
            while let Ok(mut line) = rx.recv() {
                line.push('\n');
                if let Err(e) = socket.write_all(line.as_bytes()) {
                    tracing::error!("writing to the compositor: {e}");
                    return;
                }
            }
        })
        .context("starting the socket writer")?;

    std::thread::Builder::new()
        .name("ipc-read".into())
        .spawn(move || {
            for line in BufReader::new(reader).lines() {
                let line = match line {
                    Ok(line) => line,
                    Err(e) => {
                        tracing::error!("reading from the compositor: {e}");
                        break;
                    }
                };
                if line.trim().is_empty() {
                    continue;
                }
                on_line(Line::Event(line));
            }
            on_line(Line::Closed);
        })
        .context("starting the socket reader")?;

    Ok(Sender(tx))
}

/// Whether a line is the compositor asking for a reload.
///
/// Matched on the wire rather than deserialised into `viewport_ipc::Event`:
/// every other line is destined for the page unexamined, and parsing them all
/// to recognise one would mean a shell process rejecting messages the page
/// would have understood.
pub fn is_reload(json: &str) -> bool {
    #[derive(serde::Deserialize)]
    struct Typed<'a> {
        #[serde(rename = "type")]
        kind: &'a str,
    }
    matches!(
        serde_json::from_str::<Typed>(json),
        Ok(Typed {
            kind: "shell.reload"
        })
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn a_flag_can_be_written_either_way() {
        let both = [
            args(&["shell", "--url", "http://x/", "--socket", "/tmp/s"]),
            args(&["shell", "--url=http://x/", "--socket=/tmp/s"]),
        ];
        for argv in both {
            let options = Options::parse(&argv).expect("both spellings parse");
            assert_eq!(options.url, "http://x/");
            assert_eq!(options.socket, PathBuf::from("/tmp/s"));
        }
    }

    #[test]
    fn a_page_is_required_and_says_where_it_comes_from() {
        let error = Options::parse(&args(&["shell", "--socket", "/tmp/s"]))
            .expect_err("no url is not a shell");
        let message = format!("{error}");
        assert!(message.contains("--url"), "{message}");
        assert!(message.contains("VIEWPORT_SHELL_URL"), "{message}");
    }

    #[test]
    fn only_the_reload_event_is_recognised() {
        assert!(is_reload(r#"{"type":"shell.reload"}"#));
        assert!(!is_reload(r#"{"type":"view.added","id":1}"#));
        // Everything else goes to the page untouched, including what this
        // cannot parse at all.
        assert!(!is_reload("not json"));
    }
}
