// SPDX-License-Identifier: MIT
//
// The shell, out of process, rendered by Chromium.
//
// The third engine, and the first that this compositor does not link. Chromium
// is started as a child process and driven over the DevTools protocol, which
// buys two things that matter more than they sound:
//
// * There is no engine dependency. This crate compiles in seconds against
//   serde and a pipe, on a machine with no browser installed, and the engine
//   comes from whatever `chromium` is on PATH.
//
// * The bridge needs no engine API. `Runtime.addBinding` puts a function in
//   the page that calls back out to us, and `Runtime.evaluate` puts a message
//   in — which is exactly the shape of `window.webkit.messageHandlers` and
//   `CustomEvent` that `data/shell/*.js` already speaks, once
//   `viewport_ipc::js::BRIDGE_SHIM` has hung the familiar name on it.
//
// Everything else is the WebKitGTK backend's: the window is an ordinary
// Wayland client of this compositor, on the connection the compositor made and
// handed over, so it is drawn under every window, receives every click that
// misses one, and is paced by `wl_surface::frame`. See
// `crates/viewport/src/shell_client.rs`.
//
// What this is *not* is CEF. CEF embeds the same engine as a library, with
// offscreen rendering that hands over a DMA-BUF directly — a better fit for
// the in-process backend and a much larger piece of work; see
// `crates/viewport/src/shell_backend.rs` for what it would take. This is the
// same Blink either way, which is what makes it worth having now.
//
// The transport is `--remote-debugging-pipe` rather than a port. A debugging
// *port* is a socket on the machine that anything local can connect to, and
// what it can do there is drive the desktop; the pipe is two file descriptors
// that only this process holds.

use std::io::{Read, Write};
use std::os::fd::{AsRawFd as _, OwnedFd};
use std::process::{Child, Command};
use std::sync::mpsc;

use anyhow::{anyhow, Context as _, Result};
use serde_json::{json, Value};

use viewport_shell_bridge::{Line, Options};

/// The name the page calls to reach the compositor.
///
/// `BRIDGE_SHIM` wraps it in `window.webkit.messageHandlers.viewport`, which is
/// the name the shell was written against.
const BINDING: &str = "__viewport_send";

/// Where the browser is.
fn chromium_binary() -> String {
    std::env::var("VIEWPORT_CHROMIUM_BIN").unwrap_or_else(|_| "chromium".to_owned())
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

    // A profile directory of its own, thrown away with the process. Chromium
    // refuses to share one between instances, and a shell that inherited the
    // user's browsing profile would be a desktop with their cookies in it.
    let profile = std::env::temp_dir().join(format!("viewport-shell-{}", std::process::id()));
    std::fs::create_dir_all(&profile).with_context(|| format!("creating {}", profile.display()))?;

    let mut browser = Browser::start(&options, &profile)?;
    let result = run(&mut browser, &options);

    // Whatever happened, do not leave a browser running against a compositor
    // that is no longer listening to it.
    browser.stop();
    let _ = std::fs::remove_dir_all(&profile);
    result
}

/// What the main loop is woken by.
enum Incoming {
    /// A DevTools message from the browser.
    Cdp(Value),
    /// A line from the compositor, or its socket closing.
    Compositor(Line),
    /// The browser exited.
    Gone,
}

fn run(browser: &mut Browser, options: &Options) -> Result<()> {
    let (tx, rx) = mpsc::channel::<Incoming>();

    browser.read_into(tx.clone())?;
    let out = {
        let tx = tx.clone();
        viewport_shell_bridge::connect(&options.socket, move |line| {
            let _ = tx.send(Incoming::Compositor(line));
        })?
    };

    // Which page we are attached to, and whether the bridge is in it yet.
    // Until both are true, anything from the compositor waits: a script
    // evaluated against a page that does not exist is dropped on the floor,
    // and the compositor starts talking the moment it accepts the connection.
    let mut session: Option<String> = None;
    // Chromium announces more than one target and the reply to an attach does
    // not arrive before the next announcement does. Without this the shell
    // attaches to the page three times over, installs the bridge three times,
    // and every message the page sends arrives in triplicate.
    let mut attaching = false;
    let mut ready = false;
    let mut queued: Vec<String> = Vec::new();
    let mut next_id = 1_i64;

    // Ask to be told about the page. Chromium has already created it — this is
    // an `--app` window — so the answer arrives immediately.
    browser.send(&json!({
        "id": take(&mut next_id),
        "method": "Target.setDiscoverTargets",
        "params": {"discover": true},
    }))?;

    while let Ok(event) = rx.recv() {
        match event {
            Incoming::Gone => {
                tracing::info!("the browser exited; stopping");
                return Ok(());
            }
            Incoming::Compositor(Line::Closed) => {
                tracing::info!("the compositor closed the socket; stopping");
                return Ok(());
            }
            Incoming::Compositor(Line::Event(json)) => {
                if viewport_shell_bridge::is_reload(&json) {
                    tracing::info!("reloading the shell");
                    if let Some(session) = session.as_deref() {
                        ready = false;
                        browser.call(
                            &mut next_id,
                            session,
                            "Page.reload",
                            json!({"ignoreCache": true}),
                        )?;
                    }
                    continue;
                }
                match (ready, session.as_deref()) {
                    (true, Some(session)) => browser.evaluate(
                        &mut next_id,
                        session,
                        &viewport_ipc::js::dispatch(&json),
                    )?,
                    _ => queued.push(json),
                }
            }
            Incoming::Cdp(message) => {
                if let Some(id) = attached_session(&message) {
                    tracing::info!("attached to the page");
                    install_bridge(browser, &mut next_id, &id)?;
                    session = Some(id);
                    attaching = false;
                    ready = true;
                    for json in queued.drain(..) {
                        let script = viewport_ipc::js::dispatch(&json);
                        let session = session.as_deref().expect("just set");
                        browser.evaluate(&mut next_id, session, &script)?;
                    }
                    continue;
                }

                if let Some(target) = page_target(&message) {
                    if session.is_none() && !attaching {
                        browser.send(&json!({
                            "id": take(&mut next_id),
                            "method": "Target.attachToTarget",
                            "params": {"targetId": target, "flatten": true},
                        }))?;
                        attaching = true;
                    }
                    continue;
                }

                if let Some(payload) = binding_called(&message) {
                    out.send(payload);
                    continue;
                }

                // A document was replaced — a reload, or the shell navigating.
                // The shim is re-installed by `addScriptToEvaluateOnNewDocument`
                // before any of the page's own scripts run, so this only has to
                // let messages flow again.
                if message.get("method").and_then(Value::as_str) == Some("Page.loadEventFired") {
                    ready = session.is_some();
                    continue;
                }

                if let Some(error) = message.get("error") {
                    tracing::warn!("devtools refused a command: {error}");
                }
            }
        }
    }
    Ok(())
}

/// Put the page-to-compositor half of the bridge in place, for this document
/// and every document after it.
fn install_bridge(browser: &mut Browser, next_id: &mut i64, session: &str) -> Result<()> {
    browser.call(next_id, session, "Runtime.enable", json!({}))?;
    browser.call(next_id, session, "Page.enable", json!({}))?;
    // The outbound half: a real function in the page that calls back out here
    // with whatever string it is given.
    browser.call(
        next_id,
        session,
        "Runtime.addBinding",
        json!({"name": BINDING}),
    )?;
    // The name the shell actually reaches for, wrapped around that function.
    // On every new document, before any of the page's own scripts run —
    // `data/shell/state.js` reads the handler at load time, so arriving late
    // is the same as not arriving.
    browser.call(
        next_id,
        session,
        "Page.addScriptToEvaluateOnNewDocument",
        json!({"source": viewport_ipc::js::BRIDGE_SHIM}),
    )?;
    // And once for the document that is already loaded, which the line above
    // is too late for.
    browser.evaluate(next_id, session, viewport_ipc::js::BRIDGE_SHIM)
}

fn take(next_id: &mut i64) -> i64 {
    let id = *next_id;
    *next_id += 1;
    id
}

/// The session id in a reply to `Target.attachToTarget`.
fn attached_session(message: &Value) -> Option<String> {
    message
        .get("result")?
        .get("sessionId")?
        .as_str()
        .map(str::to_owned)
}

/// The target id of a page, from `Target.targetCreated` or `Target.getTargets`.
fn page_target(message: &Value) -> Option<String> {
    let info = message.get("params")?.get("targetInfo")?;
    if info.get("type")?.as_str()? != "page" {
        return None;
    }
    info.get("targetId")?.as_str().map(str::to_owned)
}

/// The payload of a `Runtime.bindingCalled` for our binding.
fn binding_called(message: &Value) -> Option<String> {
    if message.get("method")?.as_str()? != "Runtime.bindingCalled" {
        return None;
    }
    let params = message.get("params")?;
    if params.get("name")?.as_str()? != BINDING {
        return None;
    }
    params.get("payload")?.as_str().map(str::to_owned)
}

/// The browser process, and the pipe the protocol travels over.
struct Browser {
    child: Child,
    /// Commands out. The read half is the browser's fd 3.
    write: OwnedFd,
    /// Replies and events in. The write half is the browser's fd 4.
    read: Option<OwnedFd>,
}

impl Browser {
    fn start(options: &Options, profile: &std::path::Path) -> Result<Self> {
        use rustix::pipe::pipe;

        // Two pipes, because the protocol is full duplex and a pipe is not.
        let (to_browser_read, to_browser_write) = pipe().context("making the command pipe")?;
        let (from_browser_read, from_browser_write) = pipe().context("making the event pipe")?;

        let binary = chromium_binary();
        let mut command = Command::new(&binary);
        command
            .arg("--ozone-platform=wayland")
            // No tabs, no toolbar, no omnibox: the page is the whole window.
            .arg(format!("--app={}", options.url))
            .arg(format!("--user-data-dir={}", profile.display()))
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg("--disable-search-engine-choice-screen")
            // Or the first run blocks on a keyring that a session with no
            // desktop yet has nothing to unlock it with.
            .arg("--password-store=basic")
            // fds 3 and 4 rather than a port; see the note at the top.
            .arg("--remote-debugging-pipe");

        // The GPU in the browser process rather than one of its own.
        //
        // Not a preference: with a separate GPU process, Chromium's segfaults
        // on this compositor — `GPU process exited unexpectedly: exit_code=139`
        // three times over — and it falls back to software rendering, which
        // means shared-memory buffers, which the shell element cannot draw at
        // all. In-process it produces a DMA-BUF on the first frame.
        //
        // Set `VIEWPORT_CHROMIUM_GPU_PROCESS=1` to get the separate process
        // back, which is worth trying on a machine where this is not true —
        // and worth knowing about when comparing this backend's numbers
        // against one whose engine runs its GPU work elsewhere.
        if std::env::var_os("VIEWPORT_CHROMIUM_GPU_PROCESS").is_none() {
            command.arg("--in-process-gpu");
        }
        if options.inspector {
            command.arg("--auto-open-devtools-for-tabs");
        }
        for extra in std::env::var("VIEWPORT_CHROMIUM_ARGS")
            .into_iter()
            .flat_map(|s| s.split_whitespace().map(str::to_owned).collect::<Vec<_>>())
        {
            command.arg(extra);
        }

        let child_read = to_browser_read.as_raw_fd();
        let child_write = from_browser_write.as_raw_fd();
        // SAFETY: `dup2` is async-signal-safe, which is the whole requirement
        // on a `pre_exec` closure. Nothing here allocates or takes a lock.
        //
        // The compositor's own Wayland socket is *not* touched: it arrives on
        // this process as `WAYLAND_SOCKET` with close-on-exec already cleared,
        // and is inherited by the browser untouched — which is what makes the
        // browser the shell rather than an ordinary client.
        unsafe {
            use std::os::unix::process::CommandExt as _;
            command.pre_exec(move || {
                use std::os::fd::{BorrowedFd, FromRawFd as _};
                let read = BorrowedFd::borrow_raw(child_read);
                let write = BorrowedFd::borrow_raw(child_write);
                // Owned only so that `dup2` will take them as a destination.
                // Forgotten rather than dropped: dropping closes the very
                // descriptors this just set up, and the exec is next.
                let mut three = OwnedFd::from_raw_fd(3);
                let mut four = OwnedFd::from_raw_fd(4);
                let result = rustix::io::dup2(read, &mut three)
                    .and_then(|()| rustix::io::dup2(write, &mut four));
                std::mem::forget(three);
                std::mem::forget(four);
                result.map_err(std::io::Error::from)?;
                Ok(())
            });
        }

        let child = command
            .spawn()
            .with_context(|| format!("starting {binary}"))?;
        tracing::info!("started {binary} as pid {} on {}", child.id(), options.url);

        // The child's ends belong to the child now. Holding them open here
        // would mean never seeing the browser's side of the pipe close.
        drop(to_browser_read);
        drop(from_browser_write);

        Ok(Self {
            child,
            write: to_browser_write,
            read: Some(from_browser_read),
        })
    }

    /// Read the browser's half of the protocol on a thread of its own.
    fn read_into(&mut self, tx: mpsc::Sender<Incoming>) -> Result<()> {
        let read = self
            .read
            .take()
            .ok_or_else(|| anyhow!("the event pipe is already being read"))?;
        std::thread::Builder::new()
            .name("devtools".into())
            .spawn(move || {
                let mut file = std::fs::File::from(read);
                let mut pending = Vec::new();
                let mut buffer = [0u8; 16 * 1024];
                loop {
                    let read = match file.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(n) => n,
                        Err(e) => {
                            tracing::error!("reading from the browser: {e}");
                            break;
                        }
                    };
                    pending.extend_from_slice(&buffer[..read]);
                    // Messages are NUL-terminated rather than newline
                    // delimited: the protocol carries JSON that may contain a
                    // newline inside a string, and does not escape it.
                    while let Some(end) = pending.iter().position(|byte| *byte == 0) {
                        let message: Vec<u8> = pending.drain(..=end).collect();
                        let message = &message[..message.len() - 1];
                        match serde_json::from_slice::<Value>(message) {
                            Ok(value) => {
                                if tx.send(Incoming::Cdp(value)).is_err() {
                                    return;
                                }
                            }
                            Err(e) => tracing::warn!("undecodable devtools message: {e}"),
                        }
                    }
                }
                let _ = tx.send(Incoming::Gone);
            })
            .context("starting the devtools reader")?;
        Ok(())
    }

    fn send(&mut self, message: &Value) -> Result<()> {
        let mut bytes = serde_json::to_vec(message).context("encoding a devtools command")?;
        bytes.push(0);
        let mut pipe = std::fs::File::from(
            self.write
                .try_clone()
                .context("duplicating the command pipe")?,
        );
        pipe.write_all(&bytes)
            .context("writing a devtools command")?;
        // Or the `File` closes the descriptor it was built from.
        std::mem::forget(pipe);
        Ok(())
    }

    /// A command on a page session.
    fn call(
        &mut self,
        next_id: &mut i64,
        session: &str,
        method: &str,
        params: Value,
    ) -> Result<()> {
        self.send(&json!({
            "id": take(next_id),
            "sessionId": session,
            "method": method,
            "params": params,
        }))
    }

    fn evaluate(&mut self, next_id: &mut i64, session: &str, script: &str) -> Result<()> {
        self.call(
            next_id,
            session,
            "Runtime.evaluate",
            json!({"expression": script, "awaitPromise": false, "returnByValue": false}),
        )
    }

    fn stop(&mut self) {
        // Closing the command pipe is how a browser started this way is asked
        // to exit; killing it is what happens if it will not.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_page_target_is_recognised_and_a_service_worker_is_not() {
        let page = json!({
            "method": "Target.targetCreated",
            "params": {"targetInfo": {"targetId": "abc", "type": "page"}},
        });
        assert_eq!(page_target(&page).as_deref(), Some("abc"));

        let worker = json!({
            "method": "Target.targetCreated",
            "params": {"targetInfo": {"targetId": "def", "type": "service_worker"}},
        });
        assert_eq!(page_target(&worker), None);
    }

    #[test]
    fn only_our_binding_is_treated_as_a_message() {
        let ours = json!({
            "method": "Runtime.bindingCalled",
            "params": {"name": BINDING, "payload": r#"{"type":"view.query"}"#},
        });
        assert_eq!(
            binding_called(&ours).as_deref(),
            Some(r#"{"type":"view.query"}"#)
        );

        // A page can add bindings of its own, and a compositor that forwarded
        // them would be taking instructions from whatever the page felt like
        // naming.
        let theirs = json!({
            "method": "Runtime.bindingCalled",
            "params": {"name": "somethingElse", "payload": "{}"},
        });
        assert_eq!(binding_called(&theirs), None);
    }

    #[test]
    fn a_session_id_is_read_from_the_attach_reply() {
        let reply = json!({"id": 2, "result": {"sessionId": "S1"}});
        assert_eq!(attached_session(&reply).as_deref(), Some("S1"));
        assert_eq!(attached_session(&json!({"id": 2, "result": {}})), None);
    }
}
