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

/// How much message text one `write_all` may carry.
///
/// A bound rather than a target: the batch is whatever was already queued, so
/// this only decides when to stop gathering and let the syscall go. A page that
/// posts faster than the socket drains would otherwise be able to grow this
/// buffer without limit, because the writer only stops gathering when the
/// channel is empty and a runaway producer never leaves it empty.
///
/// 64 KiB is well past a pipe's capacity and past what a layout pump produces
/// in a frame — the case this exists for is eight `view.layout` messages, a few
/// hundred bytes — so in practice the drain always ends because the channel ran
/// dry, which is the point.
const MAX_BATCH: usize = 64 * 1024;

/// One message, plus everything already queued behind it, as one write.
///
/// The shell reports the geometry of every window it lays out on every frame
/// (`data/shell/geometry.js`), so a desk with eight windows on it posts eight
/// messages within the same tick. Writing them one at a time was eight syscalls
/// per frame per window-full — around 480 a second at 60fps — for what the
/// socket is perfectly happy to take in one.
///
/// Nothing here ever *waits* for a message. `try_recv` takes what the producer
/// has already handed over and stops the moment it has not; batching that
/// blocked for a straggler would trade syscalls for exactly the input latency
/// this compositor cannot afford.
///
/// The framing on the wire is unchanged — one JSON object per line — because
/// the compositor reads this socket with a line reader (`viewport/src/ipc.rs`)
/// and neither end has any notion of a batch.
fn write_batch<W: Write>(
    first: String,
    rx: &mpsc::Receiver<String>,
    sink: &mut W,
) -> std::io::Result<()> {
    let mut buf = Vec::with_capacity(first.len() + 1);
    buf.extend_from_slice(first.as_bytes());
    buf.push(b'\n');

    while buf.len() < MAX_BATCH {
        let Ok(line) = rx.try_recv() else { break };
        buf.extend_from_slice(line.as_bytes());
        buf.push(b'\n');
    }

    // Over the bound, the caller's `recv` returns immediately with the next
    // one still queued and this starts again: full batches keep draining, they
    // just do not accumulate.
    sink.write_all(&buf)
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
            while let Ok(line) = rx.recv() {
                if let Err(e) = write_batch(line, &rx, &mut socket) {
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

    /// A sink that remembers where one `write_all` ended and the next began.
    #[derive(Default)]
    struct Writes(Vec<Vec<u8>>);

    impl Write for Writes {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.push(buf.to_vec());
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn everything_already_queued_goes_out_as_one_write() {
        let (tx, rx) = mpsc::channel();
        for id in 0..8 {
            tx.send(format!(r#"{{"type":"view.layout","id":{id}}}"#))
                .expect("the receiver is still here");
        }

        let first = rx.recv().expect("the writer thread's blocking take");
        let mut sink = Writes::default();
        write_batch(first, &rx, &mut sink).expect("the sink never fails");

        assert_eq!(sink.0.len(), 1, "a frame's worth of layout is one syscall");
        let text = String::from_utf8(sink.0[0].clone()).expect("json is utf-8");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 8);
        assert!(text.ends_with('\n'), "every message is terminated");
        for (id, line) in lines.iter().enumerate() {
            assert_eq!(*line, format!(r#"{{"type":"view.layout","id":{id}}}"#));
        }
    }

    #[test]
    fn a_lone_message_is_not_waited_on() {
        let (tx, rx) = mpsc::channel();
        tx.send(r#"{"type":"view.layout","id":1}"#.to_owned())
            .expect("the receiver is still here");

        let first = rx.recv().expect("the one message");
        let mut sink = Writes::default();
        // The channel is still open, so a batcher that waited for a second
        // message would block here rather than return.
        write_batch(first, &rx, &mut sink).expect("the sink never fails");

        assert_eq!(sink.0.len(), 1);
        assert_eq!(sink.0[0], b"{\"type\":\"view.layout\",\"id\":1}\n".to_vec());
    }

    #[test]
    fn a_runaway_producer_does_not_grow_the_buffer_without_limit() {
        let (tx, rx) = mpsc::channel();
        let message = "x".repeat(4096);
        // Far more than the bound, all queued before a single write.
        for _ in 0..64 {
            tx.send(message.clone())
                .expect("the receiver is still here");
        }

        let first = rx.recv().expect("the first of many");
        let mut sink = Writes::default();
        write_batch(first, &rx, &mut sink).expect("the sink never fails");

        let written = sink.0[0].len();
        assert_eq!(sink.0.len(), 1);
        assert!(
            written <= MAX_BATCH + message.len() + 1,
            "one message may cross the bound, a hundred may not: {written}"
        );
        // And the rest is still queued, for the next pass to take.
        assert!(rx.try_recv().is_ok());
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
