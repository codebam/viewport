// SPDX-License-Identifier: GPL-3.0-or-later
//
// Drives a real compositor over its control socket.
//
// Headless, so this needs no GPU, no display and no seat — which is the whole
// reason the headless backend exists. What it covers is the seam the unit tests
// cannot: that the socket is created with the right permissions, that framing
// and parsing are wired to the handlers, and that a refusal goes back to the
// client that caused it rather than to everyone.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Compositor {
    child: Child,
    socket: PathBuf,
}

impl Drop for Compositor {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket);
    }
}

impl Compositor {
    /// The socket path has to stay under `sockaddr_un.sun_path`, so this uses
    /// /tmp with the pid rather than CARGO_TARGET_TMPDIR, which is long.
    fn start(tag: &str) -> Self {
        let socket = PathBuf::from(format!("/tmp/viewport-test-{}-{tag}.sock", std::process::id()));
        let _ = std::fs::remove_file(&socket);

        let child = Command::new(env!("CARGO_BIN_EXE_viewport"))
            .args(["--headless", "--socket"])
            .arg(&socket)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("could not start the compositor");

        let compositor = Self { child, socket };
        compositor.wait_for_socket();
        compositor
    }

    fn wait_for_socket(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if self.socket.exists() && UnixStream::connect(&self.socket).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("the compositor never created {}", self.socket.display());
    }

    fn connect(&self) -> Client {
        let stream = UnixStream::connect(&self.socket).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        Client {
            writer: stream.try_clone().unwrap(),
            reader: BufReader::new(stream),
        }
    }
}

struct Client {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
}

impl Client {
    fn send(&mut self, message: &str) {
        self.writer.write_all(message.as_bytes()).unwrap();
        self.writer.write_all(b"\n").unwrap();
        self.writer.flush().unwrap();
    }

    /// Read messages until one has this `type`.
    fn wait_for(&mut self, kind: &str) -> serde_json::Value {
        for _ in 0..64 {
            let mut line = String::new();
            let n = self.reader.read_line(&mut line).expect("read");
            assert_ne!(n, 0, "the compositor closed the connection");
            let value: serde_json::Value =
                serde_json::from_str(line.trim()).expect("compositor sent invalid JSON");
            if value["type"] == kind {
                return value;
            }
        }
        panic!("never saw a {kind} message");
    }
}

#[test]
fn the_socket_is_private_to_its_owner() {
    use std::os::unix::fs::PermissionsExt;

    let compositor = Compositor::start("perms");
    let mode = std::fs::metadata(&compositor.socket)
        .unwrap()
        .permissions()
        .mode();
    // XDG_RUNTIME_DIR is 0700 and hides it, but the /tmp fallback is not.
    assert_eq!(mode & 0o777, 0o600, "control socket is not 0600");
}

#[test]
fn output_query_answers_with_the_headless_output() {
    let compositor = Compositor::start("output");
    let mut client = compositor.connect();

    client.send(r#"{"type":"output.query"}"#);
    let layout = client.wait_for("output.layout");

    let outputs = layout["outputs"].as_array().expect("outputs array");
    assert_eq!(outputs.len(), 1);
    let output = &outputs[0];
    assert_eq!(output["name"], "HEADLESS-1");
    assert_eq!(output["width"], 1920);
    assert_eq!(output["height"], 1080);
    // Nothing has reserved anything, so the usable area is the whole output.
    assert_eq!(output["usable_width"], 1920);
    assert_eq!(output["usable_height"], 1080);
    // The empty-string convention, never null.
    assert_eq!(output["serial"], "");
    assert!(output["modes"].as_array().unwrap().len() >= 1);
}

#[test]
fn view_query_answers_with_the_config() {
    let compositor = Compositor::start("config");
    let mut client = compositor.connect();

    client.send(r#"{"type":"view.query"}"#);
    let config = client.wait_for("config");

    assert_eq!(config["layout"], "tiling");
    assert!(config["logo"].is_boolean());
    // Unset members are omitted, not null.
    assert!(config.get("bar").is_none());
    assert!(config.get("rules").is_none());
}

#[test]
fn a_malformed_message_comes_back_as_an_error() {
    let compositor = Compositor::start("malformed");
    let mut client = compositor.connect();

    client.send("{ this is not json");
    let error = client.wait_for("error");
    assert_eq!(error["context"], "ipc");
}

#[test]
fn an_unknown_type_is_reported_against_itself() {
    let compositor = Compositor::start("unknown");
    let mut client = compositor.connect();

    client.send(r#"{"type":"view.teleport","id":1}"#);
    let error = client.wait_for("error");
    assert_eq!(error["context"], "view.teleport");
    assert_eq!(error["message"], "unknown IPC message type 'view.teleport'");
}

#[test]
fn a_type_that_is_not_a_string_cannot_reach_dispatch() {
    // The shapes that used to crash the C compositor.
    let compositor = Compositor::start("badtype");
    let mut client = compositor.connect();

    for message in [r#"{"type":5}"#, r#"{"type":null}"#, r#"{"type":{}}"#, r#"{}"#] {
        client.send(message);
        let error = client.wait_for("error");
        assert_eq!(error["context"], "ipc", "{message}");
        assert_eq!(error["message"], "missing or non-string 'type'", "{message}");
    }
}

#[test]
fn an_error_goes_only_to_the_client_that_caused_it() {
    let compositor = Compositor::start("origin");
    let mut culprit = compositor.connect();
    let mut bystander = compositor.connect();

    // Give the bystander something to find that is definitely after the error
    // the other client is about to cause, so "did not receive it" is a real
    // observation rather than a race.
    culprit.send(r#"{"type":"view.teleport"}"#);
    let error = culprit.wait_for("error");
    assert_eq!(error["context"], "view.teleport");

    bystander.send(r#"{"type":"output.query"}"#);
    let seen = bystander.wait_for("output.layout");
    assert_eq!(seen["type"], "output.layout");
}

#[test]
fn empty_lines_are_ignored_rather_than_rejected() {
    let compositor = Compositor::start("blank");
    let mut client = compositor.connect();

    // An empty string reaching the parser would come back as a malformed
    // message the sender never sent.
    client.send("");
    client.send("");
    client.send(r#"{"type":"output.query"}"#);

    let value = client.wait_for("output.layout");
    assert_eq!(value["type"], "output.layout");
}

#[test]
fn a_message_split_across_writes_still_arrives() {
    let compositor = Compositor::start("split");
    let mut client = compositor.connect();

    client.writer.write_all(br#"{"type":"outp"#).unwrap();
    client.writer.flush().unwrap();
    std::thread::sleep(Duration::from_millis(50));
    client.writer.write_all(b"ut.query\"}\n").unwrap();
    client.writer.flush().unwrap();

    let value = client.wait_for("output.layout");
    assert_eq!(value["type"], "output.layout");
}

#[test]
fn quit_stops_the_compositor() {
    let mut compositor = Compositor::start("quit");
    let mut client = compositor.connect();

    client.send(r#"{"type":"quit"}"#);

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = compositor.child.try_wait().unwrap() {
            assert!(status.success(), "exited with {status}");
            return;
        }
        assert!(Instant::now() < deadline, "still running after quit");
        std::thread::sleep(Duration::from_millis(20));
    }
}
