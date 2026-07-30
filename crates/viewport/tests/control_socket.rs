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
    /// Kept because the Wayland display name is only ever announced there.
    log: PathBuf,
}

impl Drop for Compositor {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::remove_file(&self.log);
    }
}

impl Compositor {
    /// The socket path has to stay under `sockaddr_un.sun_path`, so this uses
    /// /tmp with the pid rather than CARGO_TARGET_TMPDIR, which is long.
    fn start(tag: &str) -> Self {
        Self::start_with(tag, &[])
    }

    /// With extra environment, for the settings that only arrive that way.
    fn start_with(tag: &str, env: &[(&str, &std::path::Path)]) -> Self {
        let socket = PathBuf::from(format!(
            "/tmp/viewport-test-{}-{tag}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&socket);

        let log = PathBuf::from(format!(
            "/tmp/viewport-test-{}-{tag}.log",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&log);
        let stderr = std::fs::File::create(&log).expect("could not create the log");

        let mut command = Command::new(env!("CARGO_BIN_EXE_viewport"));
        command
            .args(["--headless", "--socket"])
            .arg(&socket)
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr));
        for (key, value) in env {
            command.env(key, value);
        }
        // Otherwise a config file in the developer's own home decides what
        // these tests see.
        if !env.iter().any(|(key, _)| *key == "XDG_CONFIG_HOME") {
            command.env("XDG_CONFIG_HOME", "/nonexistent");
        }

        let child = command.spawn().expect("could not start the compositor");

        let compositor = Self { child, socket, log };
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

    /// The Wayland display this compositor created, from its own log.
    ///
    /// Waits for it: the line is written during startup and a client pointed
    /// at a display that does not exist yet fails for a reason that has
    /// nothing to do with what is being tested.
    ///
    /// `WAYLAND_DISPLAY=<name>`, which is the same thing the shell scripts in
    /// tests/ grep for and the same shape the C build prints — one contract
    /// for both suites rather than two. Matching a bare word starting with
    /// `wayland-` would also hit Smithay's own `Created new socket
    /// name=Some("wayland-2")`, which is a different line that happens to
    /// agree.
    fn wayland_display(&self) -> Option<String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Ok(log) = std::fs::read_to_string(&self.log) {
                if let Some(name) = log
                    .split_whitespace()
                    .find_map(|word| word.strip_prefix("WAYLAND_DISPLAY="))
                {
                    return Some(
                        name.trim_end_matches(|c: char| !c.is_alphanumeric())
                            .to_owned(),
                    );
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        None
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
    assert!(!output["modes"].as_array().unwrap().is_empty());
}

#[test]
fn view_query_answers_with_the_config() {
    let compositor = Compositor::start("config");
    let mut client = compositor.connect();

    client.send(r#"{"type":"view.query"}"#);
    let config = client.wait_for("config");

    assert_eq!(config["layout"], "tiling");
    // Both true, as in src/main.c:69 — "the empty desktop explains itself
    // until told not to". These set no-logo and no-tutorial on the document
    // when false, and on a desktop with no windows they are the only things
    // there are to draw, so getting them wrong leaves bare wallpaper and
    // looks like a compositor that is not drawing the shell at all.
    //
    // Asserting is_boolean() here is what let that through.
    assert_eq!(config["logo"], true, "the empty desktop would be bare");
    assert_eq!(config["tutorial"], true, "the empty desktop would be bare");
    // Unset members are omitted, not null.
    assert!(config.get("bar").is_none());
    assert!(config.get("rules").is_none());
}

#[test]
fn a_config_file_reaches_the_shell() {
    // The point of the file: what it says is what the shell is told on
    // connect. Anything the file leaves out keeps its built-in value, which is
    // why "tutorial" is still true here.
    let dir = std::env::temp_dir().join("viewport-config-integration");
    std::fs::create_dir_all(dir.join("viewport")).expect("mkdir");
    let path = dir.join("viewport/config.json");
    std::fs::write(&path, r#"{"layout":"scrolling","logo":false,"bar":"auto"}"#).expect("write");

    let compositor = Compositor::start_with("config-file", &[("XDG_CONFIG_HOME", &dir)]);
    let mut client = compositor.connect();
    client.send(r#"{"type":"view.query"}"#);
    let config = client.wait_for("config");

    assert_eq!(config["layout"], "scrolling");
    assert_eq!(config["logo"], false);
    assert_eq!(config["bar"], "auto");
    // Not in the file, so still the built-in.
    assert_eq!(config["tutorial"], true);

    let _ = std::fs::remove_file(&path);
}

/// A real layer-shell client, run against a real compositor.
///
/// wmenu is the reason layer-shell was ported and it exercises the whole path:
/// the global has to exist, the surface has to be arranged, and the initial
/// configure has to carry a width — it asks for zero, meaning "the compositor
/// decides", and allocates a buffer from whatever it is told. Every one of
/// those was wrong at some point and each failed differently: no global was an
/// assertion inside wmenu, no configure was an invalid shm pool.
#[test]
fn a_layer_shell_client_is_configured_with_a_real_size() {
    use std::io::Read;

    let compositor = Compositor::start("layer");
    let display = compositor
        .wayland_display()
        .expect("the compositor never announced a wayland display");

    let mut child = match Command::new("wmenu")
        .env("WAYLAND_DISPLAY", &display)
        .env("WAYLAND_DEBUG", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        // Not installed. Skipping is right; failing would make the suite
        // depend on what happens to be on the machine.
        Err(_) => return,
    };
    use std::io::Write as _;
    let _ = child.stdin.take().unwrap().write_all(b"alpha\n");

    std::thread::sleep(Duration::from_secs(2));
    let _ = child.kill();
    let mut trace = String::new();
    let _ = child.stderr.take().unwrap().read_to_string(&mut trace);
    let _ = child.wait();
    // libwayland colours its trace, and it does not ask whether anything is
    // watching: the request name and its arguments come out with an escape
    // between them, so `.configure(` never appears literally and this test
    // failed on a compositor that was answering perfectly. Stripped rather
    // than matched around, because every future assertion here would have to
    // remember.
    let trace = strip_ansi(&trace);

    assert!(
        trace.contains("zwlr_layer_surface_v1"),
        "wmenu never got a layer surface: {trace}"
    );
    // The width it was told, not the zero it asked for.
    let configured = trace
        .lines()
        .find(|line| line.contains("zwlr_layer_surface_v1") && line.contains(".configure("));
    let configured = configured.unwrap_or_else(|| panic!("no configure in: {trace}"));
    assert!(
        configured.contains(", 1920, "),
        "configured with no width, so the client allocates nothing: {configured}"
    );
    assert!(
        !trace.contains("invalid wl_shm_pool size"),
        "the client was configured into an impossible buffer: {trace}"
    );
}

/// A window still gets a rectangle when nothing answers view.added.
///
/// The whole layout lives in a web page, so a shell that throws, fails to load
/// or is served from a machine that has gone away places nothing — and a
/// window that is never placed is never shown. Without this the session is a
/// black screen with a working keyboard and no way to find out why.
#[test]
fn an_unanswered_window_is_laid_out_anyway() {
    let compositor = Compositor::start("watchdog");
    let display = compositor
        .wayland_display()
        .expect("the compositor never announced a wayland display");

    // A client, and deliberately no shell: nothing will answer view.added.
    let mut client = compositor.connect();
    let mut child = match Command::new("foot")
        .env("WAYLAND_DISPLAY", &display)
        .arg("-e")
        .arg("sleep")
        .arg("30")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        // Not installed; the assertion would test nothing.
        Err(_) => return,
    };

    let added = client.wait_for("view.added");
    let id = added["id"].as_u64().expect("an id");

    // Nothing places it, so the watchdog has to. Its deadline is 2500ms.
    std::thread::sleep(Duration::from_millis(4000));
    client.send(r#"{"type":"view.query"}"#);

    // The replay carries the geometry the compositor settled on.
    let mut placed = None;
    for _ in 0..64 {
        let message = client.wait_for("view.added");
        if message["id"].as_u64() == Some(id) && message["replay"] == true {
            placed = Some(message);
            break;
        }
    }
    let _ = child.kill();
    let _ = child.wait();

    let placed = placed.expect("the window was never replayed");
    // The headless output is 1920x1080 and this is the only window, so the
    // fallback gives it the whole thing.
    assert_eq!(
        placed["width"], 1920,
        "the watchdog did not lay the window out: {placed}"
    );
}

/// The bar's numbers arrive without anyone asking.
///
/// The shell cannot read /proc — it is a page loaded from file:// — so a bar
/// with no compositor sampling behind it shows nothing at all.
#[test]
fn status_updates_arrive_on_their_own() {
    let compositor = Compositor::start("status");
    let mut client = compositor.connect();

    // Unprompted: this is a broadcast on a timer, not a reply.
    let status = client.wait_for("status.update");

    // -1 means "could not read", which is what the bar tests for. On a Linux
    // machine running this test, /proc is there.
    let memory = status["memory"].as_f64().expect("memory is a number");
    assert!(
        (0.0..=100.0).contains(&memory),
        "memory should be a percentage, got {memory}"
    );
    assert!(
        status["disk_total"].as_f64().unwrap_or(0.0) > 0.0,
        "the root filesystem has a size"
    );
    // A single number, not an array: the bar calls s.load.toFixed(2).
    assert!(status["load"].is_number(), "load must be one number");
    assert!(status["net_rx"].is_number());
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

    for message in [
        r#"{"type":5}"#,
        r#"{"type":null}"#,
        r#"{"type":{}}"#,
        r#"{}"#,
    ] {
        client.send(message);
        let error = client.wait_for("error");
        assert_eq!(error["context"], "ipc", "{message}");
        assert_eq!(
            error["message"], "missing or non-string 'type'",
            "{message}"
        );
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

/// Escape sequences out of a captured trace.
///
/// Only the CSI sequences libwayland emits — enough for a log, not a terminal
/// emulator.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        // ESC [ ... <final byte in @..~>
        if chars.next() != Some('[') {
            continue;
        }
        for c in chars.by_ref() {
            if ('@'..='~').contains(&c) {
                break;
            }
        }
    }
    out
}
