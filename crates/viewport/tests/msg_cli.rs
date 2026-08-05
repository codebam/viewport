// SPDX-License-Identifier: GPL-3.0-or-later
//
// `viewport msg` against a compositor that is really running.
//
// The unit tests in src/msg.rs cover what a command line turns into. What they
// cannot cover is the half that only exists once there is a socket on the other
// end: that a query prints its answer and stops waiting, that a refusal comes
// back as a failure rather than as silence, and that discovery finds the
// session it was told about.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

struct Compositor {
    child: Child,
    socket: PathBuf,
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
    /// Headless, so this needs no GPU, no display and no seat. In /tmp with the
    /// pid rather than CARGO_TARGET_TMPDIR, which is long enough to overrun
    /// `sockaddr_un.sun_path`.
    fn start(tag: &str) -> Self {
        let socket = PathBuf::from(format!(
            "/tmp/viewport-msg-{}-{tag}.sock",
            std::process::id()
        ));
        let log = PathBuf::from(format!(
            "/tmp/viewport-msg-{}-{tag}.log",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&socket);
        let _ = std::fs::remove_file(&log);

        let stderr = std::fs::File::create(&log).expect("could not create the log");
        let child = Command::new(env!("CARGO_BIN_EXE_viewport"))
            .args(["--headless", "--socket"])
            .arg(&socket)
            // Otherwise a config file in the developer's own home decides what
            // these tests see.
            .env("XDG_CONFIG_HOME", "/nonexistent")
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr))
            .spawn()
            .expect("could not start the compositor");

        let compositor = Self { child, socket, log };

        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if compositor.socket.exists()
                && std::os::unix::net::UnixStream::connect(&compositor.socket).is_ok()
            {
                return compositor;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!(
            "the compositor never opened {}",
            compositor.socket.display()
        );
    }

    /// `viewport msg --socket <this one> ...`, run to completion.
    fn msg(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_viewport"))
            .args(["msg", "--socket"])
            .arg(&self.socket)
            .args(args)
            // Discovery must not reach past `--socket` to a session the
            // developer is sitting in.
            .env_remove("VIEWPORT_SOCKET")
            .env_remove("WAYLAND_DISPLAY")
            .output()
            .expect("could not run viewport msg")
    }

    /// The same, left running, for `subscribe`.
    fn spawn_msg(&self, args: &[&str]) -> Child {
        Command::new(env!("CARGO_BIN_EXE_viewport"))
            .args(["msg", "--socket"])
            .arg(&self.socket)
            .args(args)
            .env_remove("VIEWPORT_SOCKET")
            .env_remove("WAYLAND_DISPLAY")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("could not run viewport msg")
    }
}

fn lines(output: &Output) -> Vec<serde_json::Value> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{line}: {e}")))
        .collect()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn a_query_prints_its_answer_and_stops() {
    let compositor = Compositor::start("output-query");

    let started = Instant::now();
    let output = compositor.msg(&["-t", "output.query"]);
    assert!(output.status.success(), "{}", stderr(&output));

    let printed = lines(&output);
    assert_eq!(printed.len(), 1, "one answer, not the whole broadcast");
    assert_eq!(printed[0]["type"], "output.layout");
    assert!(
        printed[0]["outputs"]
            .as_array()
            .is_some_and(|o| !o.is_empty()),
        "the headless backend has an output: {}",
        printed[0]
    );
    // The answer arrives at once; the timeout is only there for the case where
    // it does not.
    assert!(started.elapsed() < Duration::from_secs(2));
}

/// `view.query` is answered with a `config` and a `view.added` per window, and
/// nothing says where it ends — so the end is a gap, and the client has to stop
/// on one rather than on a message.
#[test]
fn an_answer_of_several_messages_is_printed_whole() {
    let compositor = Compositor::start("view-query");

    let output = compositor.msg(&["-t", "view.query"]);
    assert!(output.status.success(), "{}", stderr(&output));

    let printed = lines(&output);
    assert_eq!(printed[0]["type"], "config", "{printed:?}");
    // No windows in a fresh headless session, so the config is the whole of it
    // — and the unrelated broadcasts on the same socket are not printed.
    assert!(
        printed.iter().all(|event| event["type"] == "config"),
        "{printed:?}"
    );
}

#[test]
fn pretty_indents_and_plain_does_not() {
    let compositor = Compositor::start("pretty");

    let plain = compositor.msg(&["-t", "output.query"]);
    let pretty = compositor.msg(&["-t", "output.query", "--pretty"]);

    assert_eq!(String::from_utf8_lossy(&plain.stdout).lines().count(), 1);
    assert!(
        String::from_utf8_lossy(&pretty.stdout).lines().count() > 1,
        "--pretty should span lines"
    );
}

/// A message with no reply is still not a message with no answer: the handler
/// can turn it down, and a send that returned as soon as the bytes were gone
/// would report success for a message the compositor threw away.
#[test]
fn a_refusal_from_the_handler_comes_back_as_a_failure() {
    let compositor = Compositor::start("refused");

    // Well-formed, dispatched, and turned down: there is no such monitor.
    let output = compositor.msg(&["-t", "output.hdr", "--name", "NOPE-1", "--enabled", "true"]);

    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("output.hdr: no such output"),
        "{}",
        stderr(&output)
    );

    // And the ordinary case still passes, at the same cost.
    let accepted = compositor.msg(&["-t", "output.confirm"]);
    assert!(accepted.status.success(), "{}", stderr(&accepted));
}

/// `--raw` is the escape hatch, and it goes through the compositor's own parser
/// on the way out — so a message it would refuse is refused here, where the
/// complaint lands on the terminal that typed it.
#[test]
fn raw_is_checked_before_it_is_sent() {
    let compositor = Compositor::start("raw");

    let output = compositor.msg(&["--raw", r#"{"type":"view.focus","id":"seven"}"#]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("view.focus"),
        "{}",
        stderr(&output)
    );

    let unknown = compositor.msg(&["--raw", r#"{"type":"view.teleport"}"#]);
    assert_eq!(unknown.status.code(), Some(2), "{}", stderr(&unknown));
    assert!(
        stderr(&unknown).contains("view.teleport"),
        "{}",
        stderr(&unknown)
    );
}

#[test]
fn a_command_reaches_the_shell_as_a_keybinding_would() {
    let compositor = Compositor::start("shell-command");

    // Watching first, so nothing is missed between the two.
    let mut watcher = compositor.spawn_msg(&["-t", "subscribe", "shell.command"]);
    let mut reader = BufReader::new(watcher.stdout.take().expect("piped"));

    // The watcher's connection has to exist before the message is sent and
    // there is no handshake to wait on, so the message is sent over and over
    // until the read below returns — which it does on the first one that
    // arrives after the subscriber is connected.
    let socket = compositor.socket.clone();
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop = done.clone();
    let sending = std::thread::spawn(move || {
        while !stop.load(std::sync::atomic::Ordering::Relaxed) {
            let sent = Command::new(env!("CARGO_BIN_EXE_viewport"))
                .args(["msg", "--socket"])
                .arg(&socket)
                .args([
                    "-t",
                    "shell.command",
                    "--command",
                    "output.focus",
                    "--args",
                    "right",
                ])
                .env_remove("VIEWPORT_SOCKET")
                .env_remove("WAYLAND_DISPLAY")
                .output();
            match sent {
                // The compositor is gone, which means the test is over.
                Ok(sent) if !sent.status.success() => return,
                Err(_) => return,
                _ => {}
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    });

    let mut line = String::new();
    let read = reader.read_line(&mut line);
    done.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = watcher.kill();
    let _ = watcher.wait();
    let _ = sending.join();

    assert!(read.map(|n| n > 0).unwrap_or(false), "nothing was printed");
    let seen: serde_json::Value = serde_json::from_str(line.trim()).expect("JSON");

    assert_eq!(seen["type"], "shell.command");
    assert_eq!(seen["command"], "output.focus");
    assert_eq!(seen["args"][0], "right");
}

#[test]
fn subscribe_prints_only_what_it_was_asked_for() {
    let compositor = Compositor::start("subscribe");

    let mut watcher = compositor.spawn_msg(&["-t", "subscribe", "status.update"]);
    let mut reader = BufReader::new(watcher.stdout.take().expect("piped"));

    // status.update is a broadcast on a timer, so this needs no prompting.
    let mut line = String::new();
    let read = reader.read_line(&mut line);
    let _ = watcher.kill();
    let _ = watcher.wait();

    assert!(read.map(|n| n > 0).unwrap_or(false), "nothing was printed");
    let event: serde_json::Value = serde_json::from_str(line.trim()).expect("JSON");
    assert_eq!(event["type"], "status.update");
}

#[test]
fn quit_stops_the_compositor() {
    let mut compositor = Compositor::start("quit");

    let output = compositor.msg(&["-t", "quit"]);
    assert!(output.status.success(), "{}", stderr(&output));

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

#[test]
fn a_socket_with_nothing_behind_it_says_so() {
    let output = Command::new(env!("CARGO_BIN_EXE_viewport"))
        .args([
            "msg",
            "--socket",
            "/tmp/viewport-nothing-here.sock",
            "-t",
            "quit",
        ])
        .output()
        .expect("could not run viewport msg");

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("could not connect"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn help_is_help_and_not_an_error() {
    let output = Command::new(env!("CARGO_BIN_EXE_viewport"))
        .args(["msg", "--help"])
        .output()
        .expect("could not run viewport msg");

    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("usage: viewport msg"), "{text}");
    // Every message it can send is listed, because there is nowhere else to
    // look them up from a terminal.
    assert!(text.contains("view.focus"), "{text}");
    assert!(text.contains("output.configure"), "{text}");
    assert!(text.contains("subscribe"), "{text}");
}

/// The compositor's own help, which is where `msg` is discovered from.
///
/// It has to answer without a seat, a GPU or a display — asking what the
/// options are is the one thing that must work on the machine where nothing
/// else does.
#[test]
fn the_compositor_says_what_it_takes() {
    for flag in ["--help", "-h"] {
        let output = Command::new(env!("CARGO_BIN_EXE_viewport"))
            .arg(flag)
            .output()
            .expect("could not run viewport");

        assert!(output.status.success(), "{}", stderr(&output));
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(text.contains("usage: viewport"), "{text}");
        assert!(text.contains("viewport msg"), "{text}");
        assert!(text.contains("--drm"), "{text}");
        assert!(text.contains("--shell-backend"), "{text}");
        // No tracing above the answer, and nothing started.
        assert!(stderr(&output).is_empty(), "{}", stderr(&output));
    }
}

#[test]
fn a_query_with_no_compositor_behind_the_socket_times_out_rather_than_hanging() {
    // A socket that accepts and never answers, which is what a compositor
    // wedged in a render looks like from here.
    let path = format!("/tmp/viewport-msg-{}-mute.sock", std::process::id());
    let _ = std::fs::remove_file(&path);
    let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind");
    let accepting = std::thread::spawn(move || {
        // Held open until the test is done with it.
        let _kept = listener.accept();
        std::thread::sleep(Duration::from_secs(3));
    });

    let started = Instant::now();
    let output = Command::new(env!("CARGO_BIN_EXE_viewport"))
        .args([
            "msg",
            "--socket",
            &path,
            "--timeout",
            "0.5",
            "-t",
            "output.query",
        ])
        .output()
        .expect("could not run viewport msg");

    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("output.layout"),
        "{}",
        stderr(&output)
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "waited too long"
    );

    let _ = accepting.join();
    let _ = std::fs::remove_file(&path);
}

/// `$VIEWPORT_SOCKET` is what the compositor exports, so anything started from
/// a session is already pointed at that session and needs no flag.
#[test]
fn the_socket_is_found_from_the_environment() {
    let compositor = Compositor::start("discovery");

    let output = Command::new(env!("CARGO_BIN_EXE_viewport"))
        .args(["msg", "-t", "output.query"])
        .env("VIEWPORT_SOCKET", &compositor.socket)
        .output()
        .expect("could not run viewport msg");

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(lines(&output)[0]["type"], "output.layout");
}
