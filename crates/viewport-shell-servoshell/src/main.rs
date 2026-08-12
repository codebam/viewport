// SPDX-License-Identifier: MIT
//
// The shell, out of process, rendered by servoshell.
//
// Servo is not a package. It is a cargo dependency — an rlib, in a language
// with no stable ABI — so an embedding compiles the engine, which is what
// `viewport-shell-servo` does and what it costs. This crate is the other half
// of that trade: nixpkgs *does* ship a `servo` package, it installs a browser
// called `servoshell`, and a browser can be started as a child process and
// spoken to. Nothing here compiles a line of Servo, and this crate builds in
// seconds on a machine that has never had one.
//
// It is the same arrangement `chromium` has against `cef`, one engine over:
//
//     linked          driven
//     cef             chromium
//     servo           servoshell
//
// Everything outside the bridge is the WebKitGTK backend's, unchanged. The
// window is an ordinary Wayland client on the connection the compositor made
// and handed over, so it is drawn under every window, gets every click that
// misses one, and is paced by `wl_surface::frame`. See
// `crates/viewport/src/shell_client.rs`.
//
// ## The bridge, and why it is an HTTP server
//
// The `chromium` backend drives a browser over the DevTools protocol, and
// `cef` calls the same protocol through a library. Servo has two comparable
// surfaces and neither is a good bet here:
//
// * Its devtools server speaks the Firefox remote debugging protocol, whose
//   actor layout is Servo's own partial implementation and moves between
//   releases. A backend written against it is a backend that breaks when the
//   installed `servoshell` changes.
//
// * Its WebDriver server is well tested — it is what runs WPT — but a
//   WebDriver session executes one command at a time, so a long poll for
//   outbound messages is a session that cannot also deliver inbound ones.
//
// Both are TCP ports on the machine, so neither buys the isolation that made
// `--remote-debugging-pipe` worth insisting on for Chromium.
//
// What servoshell does offer is `--userscripts DIR`, which injects a script
// into every page before the page's own scripts run. That is enough to build
// the bridge out of the web platform itself: the script defines
// `window.__viewport_send` as a `fetch` at a server in *this* process, and
// long-polls the same server for events to dispatch. `viewport_ipc::js`
// supplies both ends of the shape the shell was written against —
// `BRIDGE_SHIM` hangs `window.webkit.messageHandlers.viewport` on the sender,
// and the inbound half is the same `CustomEvent('viewport')` every other
// backend delivers. `data/shell/*.js` needs no edit.
//
// The server is a `TcpListener` on 127.0.0.1 with an ephemeral port, and every
// request carries a token read from `/dev/urandom` at startup and known only
// to the injected script. That matters: what this server can do is drive the
// desktop. Loopback alone is not an access control on a multi-user machine —
// the token is.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context as _, Result};
use viewport_shell_bridge::{Line, Options};

/// How long a poll for events waits before answering "nothing yet".
///
/// Long enough that an idle desktop is not a request per second, short enough
/// that a connection is not held open past anything's idea of a dead socket.
const POLL_TIMEOUT: Duration = Duration::from_secs(25);

/// How often the main thread looks at the child it started.
const CHILD_POLL: Duration = Duration::from_millis(200);

/// Where the browser is.
fn servoshell_binary() -> String {
    std::env::var("VIEWPORT_SERVOSHELL_BIN").unwrap_or_else(|_| "servoshell".to_owned())
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();
    let options = Options::parse(&args)?;

    // Bound before the browser is started, because the port it is bound to is
    // baked into the script the browser is told to inject.
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).context("binding the bridge")?;
    let port = listener
        .local_addr()
        .context("asking which port the bridge got")?
        .port();
    let token = token().context("reading a token for the bridge")?;

    let queue = Arc::new(Queue::new());

    // The compositor first: a page that loaded before this connection existed
    // would send its first messages into a socket nobody had opened.
    let out = {
        let queue = queue.clone();
        viewport_shell_bridge::connect(&options.socket, move |line| match line {
            Line::Event(json) => {
                if viewport_shell_bridge::is_reload(&json) {
                    // There is no engine handle here to call `reload` on, so
                    // the page reloads itself. The user script is injected
                    // again with it, which is what makes this survivable at
                    // all — the bridge is re-established by the same means it
                    // was established.
                    queue.push(Item::Reload);
                } else {
                    queue.push(Item::Event(json));
                }
            }
            Line::Closed => queue.close(),
        })?
    };

    let bridge = Arc::new(Bridge {
        queue: queue.clone(),
        token,
        out,
    });
    serve(listener, bridge.clone())?;

    let scripts = ScriptDir::write(&bridge_script(port, &bridge.token))?;
    let mut child = start(&options, scripts.path())?;

    let status = loop {
        if let Some(status) = child.try_wait().context("waiting for servoshell")? {
            break status;
        }
        if queue.is_closed() {
            tracing::info!("the compositor closed the socket; stopping servoshell");
            let _ = child.kill();
            break child.wait().context("reaping servoshell")?;
        }
        std::thread::sleep(CHILD_POLL);
    };

    tracing::info!("servoshell exited: {status}");
    Ok(())
}

/// Start the browser on the connection this process was handed.
///
/// The compositor's Wayland socket arrives here as `WAYLAND_SOCKET` with
/// close-on-exec already cleared, and is inherited untouched — which is what
/// makes the browser the shell rather than an ordinary client.
fn start(options: &Options, scripts: &Path) -> Result<Child> {
    let binary = servoshell_binary();
    let mut command = Command::new(&binary);
    // `--flag=value`, which is how servoshell's own help spells every one of
    // its options.
    command.arg(format!("--userscripts={}", scripts.display()));

    if options.inspector {
        // A port, because servoshell has no pipe transport. Loopback, and only
        // when asked for: this is the shell being worked on, not the shell
        // running a session.
        command.arg("--devtools=127.0.0.1:6080");
    }
    for extra in std::env::var("VIEWPORT_SERVOSHELL_ARGS")
        .into_iter()
        .flat_map(|s| s.split_whitespace().map(str::to_owned).collect::<Vec<_>>())
    {
        command.arg(extra);
    }
    // Last, and positional: everything after it would be read as another URL.
    command.arg(&options.url);

    let child = command
        .spawn()
        .with_context(|| format!("starting {binary}"))?;
    tracing::info!("started {binary} as pid {} on {}", child.id(), options.url);
    Ok(child)
}

// ---------------------------------------------------------------------------
// The queue between the compositor's socket and the page's poll.
// ---------------------------------------------------------------------------

/// One thing the page has to be told.
enum Item {
    /// An event, as JSON, exactly as it came off the compositor's socket.
    Event(String),
    /// Reload the page. See the `is_reload` call site.
    Reload,
}

impl Item {
    /// The item as the injected script reads it.
    fn to_json(&self) -> serde_json::Value {
        match self {
            Item::Event(json) => serde_json::json!({"kind": "event", "data": json}),
            Item::Reload => serde_json::json!({"kind": "reload"}),
        }
    }
}

/// Everything the page has been told, and whether there is any more coming.
///
/// Kept as a growing log rather than a drained channel because the page asks
/// for "everything after N": a reload starts a new poll at 0 and a request
/// that died in flight is retried without losing what it had claimed. The log
/// is trimmed by nothing, which is right for a session's worth of layout
/// events and would not be for a stream.
struct Queue {
    state: Mutex<QueueState>,
    changed: Condvar,
}

#[derive(Default)]
struct QueueState {
    items: Vec<Item>,
    closed: bool,
}

impl Queue {
    fn new() -> Self {
        Self {
            state: Mutex::new(QueueState::default()),
            changed: Condvar::new(),
        }
    }

    fn push(&self, item: Item) {
        let mut state = self.state.lock().expect("the queue lock is never poisoned");
        state.items.push(item);
        drop(state);
        self.changed.notify_all();
    }

    fn close(&self) {
        let mut state = self.state.lock().expect("the queue lock is never poisoned");
        state.closed = true;
        drop(state);
        self.changed.notify_all();
    }

    fn is_closed(&self) -> bool {
        self.state
            .lock()
            .expect("the queue lock is never poisoned")
            .closed
    }

    /// Everything after `after`, waiting up to [`POLL_TIMEOUT`] for it.
    ///
    /// Returns the new high-water mark and the items, which is empty when the
    /// wait timed out — the page asks again with the same mark.
    fn since(&self, after: usize) -> (usize, Vec<serde_json::Value>) {
        let state = self.state.lock().expect("the queue lock is never poisoned");
        let (state, _) = self
            .changed
            .wait_timeout_while(state, POLL_TIMEOUT, |state| {
                state.items.len() <= after && !state.closed
            })
            .expect("the queue lock is never poisoned");
        let from = after.min(state.items.len());
        (
            state.items.len(),
            state.items[from..].iter().map(Item::to_json).collect(),
        )
    }
}

// ---------------------------------------------------------------------------
// The bridge server.
// ---------------------------------------------------------------------------

struct Bridge {
    queue: Arc<Queue>,
    /// The shared secret in every request. See the header.
    token: String,
    /// The page-to-compositor direction.
    out: viewport_shell_bridge::Sender,
}

/// 32 hex characters of `/dev/urandom`.
///
/// Not a crate: this is the one random number this process needs, and the
/// kernel's own source of them is a file.
fn token() -> Result<String> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .context("opening /dev/urandom")?
        .read_exact(&mut bytes)
        .context("reading /dev/urandom")?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn serve(listener: TcpListener, bridge: Arc<Bridge>) -> Result<()> {
    std::thread::Builder::new()
        .name("bridge".into())
        .spawn(move || {
            for stream in listener.incoming() {
                let stream = match stream {
                    Ok(stream) => stream,
                    Err(e) => {
                        tracing::error!("accepting a bridge connection: {e}");
                        continue;
                    }
                };
                let bridge = bridge.clone();
                // One thread per connection, and there are two connections:
                // the poll that is always outstanding and whatever `send` is
                // in flight. A pool would be a pool of two.
                if let Err(e) = std::thread::Builder::new()
                    .name("bridge-conn".into())
                    .spawn(move || {
                        if let Err(e) = handle(stream, &bridge) {
                            tracing::debug!("a bridge connection ended: {e}");
                        }
                    })
                {
                    tracing::error!("starting a bridge connection thread: {e}");
                }
            }
        })
        .context("starting the bridge")?;
    Ok(())
}

fn handle(stream: TcpStream, bridge: &Bridge) -> Result<()> {
    let mut reader = BufReader::new(stream.try_clone().context("duplicating the socket")?);
    let mut request_line = String::new();
    if reader
        .read_line(&mut request_line)
        .context("reading a request")?
        == 0
    {
        return Ok(());
    }
    let (method, target) = parse_request_line(&request_line)
        .ok_or_else(|| anyhow!("unparseable request line: {request_line:?}"))?;

    let mut length = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).context("reading a header")? == 0 {
            break;
        }
        if header.trim().is_empty() {
            break;
        }
        if let Some(value) = header_value(&header, "content-length") {
            length = value.trim().parse().unwrap_or(0);
        }
    }

    let mut body = vec![0u8; length];
    if length > 0 {
        reader.read_exact(&mut body).context("reading a body")?;
    }

    let path = target.split('?').next().unwrap_or("").to_owned();

    // A cross-origin `fetch` from the shell page, whatever origin the page was
    // loaded from — `file://` sends `Origin: null`. Answered for every route
    // so that a mistyped one fails as a 404 rather than as a CORS error, which
    // is the difference between a debuggable shell and a silent one.
    if method == "OPTIONS" {
        return respond(&stream, 204, "", "");
    }

    if query_value(&target, "t").as_deref() != Some(bridge.token.as_str()) {
        tracing::warn!("a request to {path} arrived without this session's token");
        return respond(&stream, 403, "text/plain", "no");
    }

    match (method.as_str(), path.as_str()) {
        ("POST", "/send") => {
            let message = String::from_utf8(body).context("a message that is not UTF-8")?;
            bridge.out.send(message);
            respond(&stream, 204, "", "")
        }
        ("GET", "/events") => {
            let after = query_value(&target, "after")
                .and_then(|value| value.parse().ok())
                .unwrap_or(0);
            let (next, items) = bridge.queue.since(after);
            if items.is_empty() {
                return respond(&stream, 204, "", "");
            }
            let batch = serde_json::json!({"next": next, "items": items});
            respond(&stream, 200, "application/json", &batch.to_string())
        }
        _ => respond(&stream, 404, "text/plain", "no such thing"),
    }
}

fn respond(mut stream: &TcpStream, status: u16, content_type: &str, body: &str) -> Result<()> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        403 => "Forbidden",
        _ => "Not Found",
    };
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Access-Control-Allow-Headers: content-type\r\n\
         Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n\
         Cache-Control: no-store\r\n\
         Content-Length: {}\r\n",
        body.len()
    );
    if !content_type.is_empty() {
        head.push_str(&format!("Content-Type: {content_type}\r\n"));
    }
    head.push_str("Connection: close\r\n\r\n");
    stream
        .write_all(head.as_bytes())
        .and_then(|()| stream.write_all(body.as_bytes()))
        .context("answering a bridge request")
}

/// `GET /events?t=... HTTP/1.1` into its method and its target.
fn parse_request_line(line: &str) -> Option<(String, String)> {
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_owned();
    let target = parts.next()?.to_owned();
    Some((method, target))
}

/// One header's value, if the line is that header.
fn header_value(line: &str, name: &str) -> Option<String> {
    let (header, value) = line.split_once(':')?;
    header
        .trim()
        .eq_ignore_ascii_case(name)
        .then(|| value.trim().to_owned())
}

/// One query parameter, undecoded.
///
/// Undecoded is correct rather than lazy: the two parameters this server takes
/// are a hex token and a decimal number, and a request carrying anything that
/// would need decoding is a request that fails the token check.
fn query_value(target: &str, name: &str) -> Option<String> {
    let (_, query) = target.split_once('?')?;
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == name).then(|| value.to_owned())
    })
}

// ---------------------------------------------------------------------------
// The injected script.
// ---------------------------------------------------------------------------

/// The directory `--userscripts` is pointed at, and its removal.
struct ScriptDir(PathBuf);

impl ScriptDir {
    fn write(script: &str) -> Result<Self> {
        let dir =
            std::env::temp_dir().join(format!("viewport-shell-servoshell-{}", std::process::id()));
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        std::fs::write(dir.join("viewport-bridge.js"), script)
            .with_context(|| format!("writing the bridge script into {}", dir.display()))?;
        Ok(Self(dir))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScriptDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The script servoshell injects into the page before the page's own scripts.
///
/// Two halves and a shim. The sender is a `fetch` at this process;
/// `viewport_ipc::js::BRIDGE_SHIM` wraps it in the
/// `window.webkit.messageHandlers.viewport` name `data/shell/state.js` calls;
/// the pump is a long poll that dispatches the same `CustomEvent('viewport')`
/// every other backend delivers.
fn bridge_script(port: u16, token: &str) -> String {
    let shim = viewport_ipc::js::BRIDGE_SHIM;
    format!(
        r#"(function () {{
  'use strict';
  var ORIGIN = 'http://127.0.0.1:{port}';
  var TOKEN = '{token}';

  window.__viewport_send = function (message) {{
    /* `keepalive` so that a message sent while the page is going away — a
       reload, a navigation — is still delivered. */
    fetch(ORIGIN + '/send?t=' + TOKEN, {{
      method: 'POST',
      body: message,
      cache: 'no-store',
      keepalive: true,
    }}).catch(function (e) {{
      console.error('viewport: the compositor bridge refused a message', e);
    }});
  }};

{shim}

  var after = 0;
  function pump() {{
    fetch(ORIGIN + '/events?t=' + TOKEN + '&after=' + after, {{cache: 'no-store'}})
      .then(function (response) {{
        return response.status === 204 ? null : response.json();
      }})
      .then(function (batch) {{
        if (batch) {{
          after = batch.next;
          batch.items.forEach(function (item) {{
            if (item.kind === 'reload') {{
              window.location.reload();
              return;
            }}
            window.dispatchEvent(new CustomEvent('viewport', {{
              detail: JSON.parse(item.data),
            }}));
          }});
        }}
        pump();
      }})
      /* The compositor going away closes this server with it, so a failed
         poll is retried rather than treated as the end: the shell process is
         restarted by the compositor, and the page should reconnect to it
         rather than sit there having given up. */
      .catch(function () {{
        setTimeout(pump, 250);
      }});
  }}
  pump();
}})();
"#
    )
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixListener;
    use std::sync::mpsc;

    use super::*;

    /// A compositor that accepts one connection and hands back what it was
    /// told, so the outbound half can be tested without a compositor.
    fn fake_compositor() -> (PathBuf, mpsc::Receiver<String>) {
        let path = std::env::temp_dir().join(format!(
            "viewport-servoshell-test-{}-{:?}.sock",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("a socket to listen on");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("the shell connects");
            for line in BufReader::new(stream).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        (path, rx)
    }

    /// One HTTP request, and everything that came back.
    fn request(port: u16, head: &str, body: &str) -> String {
        use std::net::TcpStream;
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).expect("the bridge");
        let request = format!(
            "{head} HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        stream
            .write_all(request.as_bytes())
            .expect("writing a request");
        let mut answer = String::new();
        stream.read_to_string(&mut answer).expect("an answer");
        answer
    }

    /// The whole bridge, both directions, over a real socket.
    ///
    /// This is the test that would have caught every mistake worth making
    /// here — a route spelled differently in the script than in the server, a
    /// body that never reaches the compositor, a poll that answers before the
    /// event it was waiting for.
    #[test]
    fn a_message_reaches_the_compositor_and_an_event_reaches_the_page() {
        let (socket, from_page) = fake_compositor();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("a port");
        let port = listener.local_addr().expect("the port").port();

        let queue = Arc::new(Queue::new());
        let out = {
            let queue = queue.clone();
            viewport_shell_bridge::connect(&socket, move |line| match line {
                Line::Event(json) => queue.push(Item::Event(json)),
                Line::Closed => queue.close(),
            })
            .expect("connecting to the fake compositor")
        };
        let bridge = Arc::new(Bridge {
            queue: queue.clone(),
            token: "secret".to_owned(),
            out,
        });
        serve(listener, bridge).expect("the bridge starts");

        // Outbound: what the page posts is what the compositor reads.
        let answer = request(port, "POST /send?t=secret", r#"{"type":"view.focus"}"#);
        assert!(answer.starts_with("HTTP/1.1 204"), "{answer}");
        assert_eq!(
            from_page
                .recv_timeout(Duration::from_secs(5))
                .expect("the compositor hears the page"),
            r#"{"type":"view.focus"}"#
        );

        // Inbound: what the compositor said is what the poll hands over.
        queue.push(Item::Event(r#"{"type":"view.added"}"#.to_owned()));
        let answer = request(port, "GET /events?t=secret&after=0", "");
        assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
        assert!(answer.contains(r#""kind":"event""#), "{answer}");
        assert!(answer.contains("view.added"), "{answer}");
        // Whatever the page's origin is, it has to be allowed to read this.
        assert!(
            answer.contains("Access-Control-Allow-Origin: *"),
            "{answer}"
        );

        let _ = std::fs::remove_file(&socket);
    }

    /// Anything else on the machine can open a loopback socket. What stops it
    /// driving the desktop is this.
    #[test]
    fn a_request_without_the_token_is_refused() {
        let (socket, from_page) = fake_compositor();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("a port");
        let port = listener.local_addr().expect("the port").port();

        let queue = Arc::new(Queue::new());
        let out = viewport_shell_bridge::connect(&socket, |_| {}).expect("connecting");
        serve(
            listener,
            Arc::new(Bridge {
                queue,
                token: "secret".to_owned(),
                out,
            }),
        )
        .expect("the bridge starts");

        let answer = request(port, "POST /send?t=guess", r#"{"type":"view.focus"}"#);
        assert!(answer.starts_with("HTTP/1.1 403"), "{answer}");
        assert!(
            from_page.recv_timeout(Duration::from_millis(250)).is_err(),
            "a message with no token reached the compositor"
        );

        let _ = std::fs::remove_file(&socket);
    }

    #[test]
    fn a_request_line_is_its_method_and_target() {
        assert_eq!(
            parse_request_line("GET /events?t=abc&after=3 HTTP/1.1\r\n"),
            Some(("GET".to_owned(), "/events?t=abc&after=3".to_owned()))
        );
        assert_eq!(parse_request_line("\r\n"), None);
    }

    #[test]
    fn a_header_is_matched_whatever_its_case() {
        assert_eq!(
            header_value("Content-Length: 12\r\n", "content-length"),
            Some("12".to_owned())
        );
        assert_eq!(header_value("Origin: null\r\n", "content-length"), None);
    }

    #[test]
    fn a_query_parameter_is_found_wherever_it_is() {
        assert_eq!(
            query_value("/events?t=abc&after=3", "after"),
            Some("3".to_owned())
        );
        assert_eq!(query_value("/send?t=abc", "t"), Some("abc".to_owned()));
        assert_eq!(query_value("/send", "t"), None);
    }

    /// The token is what stands between this server and anything else on the
    /// machine that can open a loopback socket, so it has to be in the script
    /// and it has to be unpredictable.
    #[test]
    fn the_script_carries_the_token_and_the_port() {
        let script = bridge_script(4321, "deadbeef");
        assert!(script.contains("http://127.0.0.1:4321"), "{script}");
        assert!(script.contains("'deadbeef'"), "{script}");
    }

    #[test]
    fn two_tokens_are_not_the_same_token() {
        let (first, second) = (token().expect("urandom"), token().expect("urandom"));
        assert_eq!(first.len(), 32);
        assert_ne!(first, second);
    }

    /// The page is written against WebKit's name for the bridge, and this
    /// backend has no engine API to register it with — so the shim is the only
    /// thing that makes `postMessage` exist at all.
    #[test]
    fn the_script_installs_the_name_the_shell_calls() {
        let script = bridge_script(1, "t");
        assert!(script.contains("window.__viewport_send"));
        assert!(script.contains("window.webkit.messageHandlers.viewport"));
        assert!(script.contains("new CustomEvent('viewport'"));
    }

    #[test]
    fn a_queue_hands_back_only_what_is_new() {
        let queue = Queue::new();
        queue.push(Item::Event(r#"{"type":"view.added"}"#.to_owned()));
        queue.push(Item::Reload);

        let (next, items) = queue.since(0);
        assert_eq!(next, 2);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["kind"], "event");
        assert_eq!(items[0]["data"], r#"{"type":"view.added"}"#);
        assert_eq!(items[1]["kind"], "reload");

        let (next, items) = queue.since(1);
        assert_eq!(next, 2);
        assert_eq!(items.len(), 1);
    }

    /// A poll outstanding when the compositor goes away has to come back, or
    /// the page holds a request open against a server about to be torn down
    /// and the shell process cannot exit.
    #[test]
    fn closing_the_socket_wakes_a_poll() {
        let queue = Arc::new(Queue::new());
        let waiter = queue.clone();
        let handle = std::thread::spawn(move || waiter.since(0));
        std::thread::sleep(Duration::from_millis(50));
        queue.close();
        let (_, items) = handle.join().expect("the poll returns");
        assert!(items.is_empty());
        assert!(queue.is_closed());
    }
}
