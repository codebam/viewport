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

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};

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
pub struct Sender(Arc<Outbound>);

/// Page messages waiting for the socket writer.
///
/// This is byte-bounded rather than message-bounded: a layout is a few hundred
/// bytes, but the protocol permits a line close to a megabyte, so a queue of a
/// fixed number of messages is not a useful memory limit.
struct Outbound {
    queue: Mutex<Queued>,
    ready: Condvar,
    socket: UnixStream,
}

#[derive(Default)]
struct Queued {
    messages: VecDeque<String>,
    bytes: usize,
    closed: bool,
}

/// Enough for many frames of ordinary geometry without allowing a wedged
/// compositor to turn the shell process into an unbounded buffer.
const MAX_QUEUED: usize = 4 * 1024 * 1024;

impl Sender {
    /// Queue one already-serialised message without blocking the engine.
    ///
    /// Overflow closes the write half of the control socket. Dropping an
    /// arbitrary command would leave the page and compositor with different
    /// state; disconnecting instead lets the compositor restart the shell from
    /// a known snapshot.
    pub fn send(&self, json: String) {
        let size = json.len().saturating_add(1);
        let mut queue = self.0.queue.lock().unwrap_or_else(|e| e.into_inner());
        if queue.closed {
            return;
        }
        if size > MAX_QUEUED || queue.bytes.saturating_add(size) > MAX_QUEUED {
            queue.closed = true;
            queue.messages.clear();
            queue.bytes = 0;
            drop(queue);
            let _ = self.0.socket.shutdown(Shutdown::Write);
            self.0.ready.notify_one();
            tracing::error!(
                "the compositor is not draining shell messages; closing the control socket"
            );
            return;
        }
        queue.bytes += size;
        queue.messages.push_back(json);
        drop(queue);
        self.0.ready.notify_one();
    }
}

/// How much message text one `write_all` may carry.
///
/// A bound rather than a target: the batch is whatever was already queued, so
/// this only decides when to stop gathering and let the syscall go. The queue
/// itself has the separate byte bound above.
///
/// 64 KiB is well past what a layout pump produces in a frame — the usual case
/// is eight `view.layout` messages, a few hundred bytes — while still giving
/// another producer a chance to enqueue between large writes.
const MAX_BATCH: usize = 64 * 1024;

/// One message, plus everything already queued behind it, as one write.
///
/// The shell reports the geometry of every window it lays out on every frame
/// (`data/shell/geometry.js`), so a desk with eight windows on it posts eight
/// messages within the same tick. Writing them one at a time was eight syscalls
/// per frame per window-full — around 480 a second at 60fps — for what the
/// socket is perfectly happy to take in one.
///
/// Waiting happens only when the queue is empty. Once the first message is
/// present, the batch takes what is already there and never waits for a
/// straggler; doing so would trade syscalls for input latency.
///
/// The framing on the wire is unchanged — one JSON object per line — because
/// the compositor reads this socket with a line reader (`viewport/src/ipc.rs`)
/// and neither end has any notion of a batch.
fn take_batch(outbound: &Outbound) -> Option<Vec<u8>> {
    let mut queue = outbound.queue.lock().unwrap_or_else(|e| e.into_inner());
    while queue.messages.is_empty() && !queue.closed {
        queue = outbound
            .ready
            .wait(queue)
            .unwrap_or_else(|e| e.into_inner());
    }
    if queue.closed {
        return None;
    }

    let mut buf = Vec::new();
    while buf.len() < MAX_BATCH {
        let Some(line) = queue.messages.pop_front() else {
            break;
        };
        queue.bytes = queue.bytes.saturating_sub(line.len() + 1);
        buf.extend_from_slice(line.as_bytes());
        buf.push(b'\n');
    }
    Some(buf)
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
    let outbound = Arc::new(Outbound {
        queue: Mutex::new(Queued::default()),
        ready: Condvar::new(),
        socket: socket
            .try_clone()
            .context("duplicating the socket for overflow handling")?,
    });

    let writer = outbound.clone();
    std::thread::Builder::new()
        .name("ipc-write".into())
        .spawn(move || {
            let mut socket = socket;
            while let Some(batch) = take_batch(&writer) {
                if let Err(e) = socket.write_all(&batch) {
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

    Ok(Sender(outbound))
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

    fn sender() -> (Sender, Arc<Outbound>) {
        let (socket, _peer) = UnixStream::pair().expect("a socket pair");
        let outbound = Arc::new(Outbound {
            queue: Mutex::new(Queued::default()),
            ready: Condvar::new(),
            socket,
        });
        (Sender(outbound.clone()), outbound)
    }

    #[test]
    fn everything_already_queued_goes_out_as_one_write() {
        let (sender, outbound) = sender();
        for id in 0..8 {
            sender.send(format!(r#"{{"type":"view.layout","id":{id}}}"#));
        }

        let batch = take_batch(&outbound).expect("a batch");
        let text = String::from_utf8(batch).expect("json is utf-8");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 8);
        assert!(text.ends_with('\n'), "every message is terminated");
        for (id, line) in lines.iter().enumerate() {
            assert_eq!(*line, format!(r#"{{"type":"view.layout","id":{id}}}"#));
        }
    }

    #[test]
    fn a_lone_message_is_not_waited_on() {
        let (sender, outbound) = sender();
        sender.send(r#"{"type":"view.layout","id":1}"#.to_owned());

        assert_eq!(
            take_batch(&outbound).expect("the one message"),
            b"{\"type\":\"view.layout\",\"id\":1}\n".to_vec()
        );
    }

    #[test]
    fn a_runaway_producer_closes_before_the_queue_can_grow_without_limit() {
        let (sender, outbound) = sender();
        sender.send("x".repeat(MAX_QUEUED));

        let queue = outbound.queue.lock().expect("the queue");
        assert!(queue.closed);
        assert_eq!(queue.bytes, 0);
        assert!(queue.messages.is_empty());
    }

    #[test]
    fn a_batch_is_bounded_even_when_more_is_queued() {
        let (sender, outbound) = sender();
        let message = "x".repeat(4096);
        for _ in 0..64 {
            sender.send(message.clone());
        }

        let written = take_batch(&outbound).expect("a batch").len();
        assert!(written <= MAX_BATCH + message.len() + 1);
        assert!(!outbound
            .queue
            .lock()
            .expect("the queue")
            .messages
            .is_empty());
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
