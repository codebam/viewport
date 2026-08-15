// SPDX-License-Identifier: GPL-3.0-or-later
//
// One compositor harness for every integration test.
//
// Each of these suites needs the same thing: a real compositor, started
// headless so it needs no GPU, no display and no seat, with a control socket
// this process owns and a log it can read back. What they disagree about is
// only the edges — extra flags, extra environment, a directory of their own, a
// line to wait for before the test begins — so those are the parameters and
// everything else lives here.
//
// This file is `common/mod.rs` rather than `common.rs` because cargo compiles
// every top-level file in tests/ as its own test binary, and a harness with no
// #[test] in it would be an empty one. A subdirectory is not a target, so it is
// only ever a module of the suites that say `mod common;`.

// Every suite uses a different part of this, and the parts it does not use are
// still used by its neighbours.
#![allow(dead_code)]

use std::ffi::{OsStr, OsString};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub struct Compositor {
    pub child: Child,
    pub socket: PathBuf,
    /// Kept because the Wayland display name is only ever announced there.
    pub log: PathBuf,
    /// A directory the harness made for this test and takes away again.
    directory: Option<PathBuf>,
}

impl Drop for Compositor {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::remove_file(&self.log);
        if let Some(directory) = &self.directory {
            let _ = std::fs::remove_dir_all(directory);
        }
    }
}

impl Compositor {
    /// A headless compositor with nothing but a socket.
    pub fn start(tag: &str) -> Self {
        Self::builder(tag).start()
    }

    /// The same, with whatever extra arguments the test is about.
    pub fn start_with_args(tag: &str, args: &[&str]) -> Self {
        Self::builder(tag).args(args).start()
    }

    /// For the tests that need more than a tag: extra environment, a
    /// directory of their own, a startup line to wait for.
    pub fn builder(tag: &str) -> Builder {
        Builder {
            prefix: "viewport-test",
            tag: tag.to_owned(),
            args: Vec::new(),
            env: Vec::new(),
            directory: None,
            awaited: None,
        }
    }

    /// The directory this compositor was given, for the tests that write into
    /// what the compositor is watching.
    pub fn directory(&self) -> &Path {
        self.directory
            .as_deref()
            .expect("this compositor was not given a directory")
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

    pub fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// Wait for a line in the log, and say whether it turned up.
    pub fn saw(&self, needle: &str, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if let Ok(log) = std::fs::read_to_string(&self.log) {
                if log.contains(needle) {
                    return true;
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    /// Wait for a line in the log, and say how long it took.
    ///
    /// The same watching as `saw`, polled more finely and answering with the
    /// delay: the shell-watch suite asserts on how prompt a reload was, and a
    /// 50ms poll cannot tell a debounce of 200ms from one of 400.
    pub fn wait_for(&self, needle: &str, patience: Duration) -> Option<Duration> {
        let started = Instant::now();
        while started.elapsed() < patience {
            if let Ok(log) = std::fs::read_to_string(&self.log) {
                if log.contains(needle) {
                    return Some(started.elapsed());
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        None
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
    pub fn wayland_display(&self) -> Option<String> {
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

    pub fn connect(&self) -> Client {
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

pub struct Builder {
    prefix: &'static str,
    tag: String,
    args: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
    directory: Option<PathBuf>,
    awaited: Option<(String, Duration)>,
}

impl Builder {
    /// The leading part of the socket, log and directory names.
    ///
    /// Suites that run at the same time must not collide, and the pid alone
    /// does not separate them: cargo runs one process per test binary, so two
    /// suites using the same tag would be two compositors on one socket.
    pub fn prefix(mut self, prefix: &'static str) -> Self {
        self.prefix = prefix;
        self
    }

    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.args.push(arg.as_ref().to_owned());
        self
    }

    pub fn args<S: AsRef<OsStr>>(mut self, args: impl IntoIterator<Item = S>) -> Self {
        self.args
            .extend(args.into_iter().map(|arg| arg.as_ref().to_owned()));
        self
    }

    /// For the settings that only arrive as environment.
    pub fn env(mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
        self.env
            .push((key.as_ref().to_owned(), value.as_ref().to_owned()));
        self
    }

    /// A directory this compositor is to be given, and that goes when it does.
    pub fn owning(mut self, directory: PathBuf) -> Self {
        self.directory = Some(directory);
        self
    }

    /// A line to wait for in the log before handing the compositor over.
    ///
    /// For the suites where startup is not finished when the socket appears —
    /// a test that writes to a directory before the watch on it has been taken
    /// is testing the race, not the watch.
    pub fn awaiting(mut self, needle: &str, patience: Duration) -> Self {
        self.awaited = Some((needle.to_owned(), patience));
        self
    }

    /// The socket path has to stay under `sockaddr_un.sun_path`, so this uses
    /// /tmp with the pid rather than CARGO_TARGET_TMPDIR, which is long.
    pub fn start(self) -> Compositor {
        let Builder {
            prefix,
            tag,
            args,
            env,
            directory,
            awaited,
        } = self;
        let pid = std::process::id();

        let socket = PathBuf::from(format!("/tmp/{prefix}-{pid}-{tag}.sock"));
        let _ = std::fs::remove_file(&socket);

        let log = PathBuf::from(format!("/tmp/{prefix}-{pid}-{tag}.log"));
        let _ = std::fs::remove_file(&log);
        let stderr = std::fs::File::create(&log).expect("could not create the log");

        let mut command = Command::new(env!("CARGO_BIN_EXE_viewport"));
        command
            .args(["--headless", "--socket"])
            .arg(&socket)
            .args(&args)
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr));
        for (key, value) in &env {
            command.env(key, value);
        }
        // Otherwise a config file in the developer's own home decides what
        // these tests see.
        if !env.iter().any(|(key, _)| key == "XDG_CONFIG_HOME") {
            command.env("XDG_CONFIG_HOME", "/nonexistent");
        }

        let child = command.spawn().expect("could not start the compositor");

        let compositor = Compositor {
            child,
            socket,
            log,
            directory,
        };
        compositor.wait_for_socket();
        if let Some((needle, patience)) = awaited {
            compositor
                .wait_for(&needle, patience)
                .unwrap_or_else(|| panic!("the compositor never said {needle:?}"));
        }
        compositor
    }
}

pub struct Client {
    pub writer: UnixStream,
    reader: BufReader<UnixStream>,
}

impl Client {
    pub fn send(&mut self, message: &str) {
        self.writer.write_all(message.as_bytes()).unwrap();
        self.writer.write_all(b"\n").unwrap();
        self.writer.flush().unwrap();
    }

    /// Read messages until one has this `type`.
    pub fn wait_for(&mut self, kind: &str) -> serde_json::Value {
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

    /// Everything readable within a moment, as lines.
    ///
    /// Time-bounded rather than counted: the point of most of these
    /// assertions is that a particular message never arrives, and there is no
    /// count that says "and nothing else".
    pub fn drain_lines(&mut self, within: Duration) -> Vec<String> {
        let deadline = Instant::now() + within;
        let mut lines = Vec::new();
        self.writer
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        while Instant::now() < deadline {
            let mut line = String::new();
            match self.reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => lines.push(line.trim_end().to_owned()),
                Err(_) => continue,
            }
        }
        lines
    }

    /// The same, as parsed JSON, dropping anything that is not.
    pub fn drain(&mut self, within: Duration) -> Vec<serde_json::Value> {
        self.drain_lines(within)
            .iter()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    /// Ask for the config and hand back the one that comes past.
    ///
    /// `view.query` is the request a shell makes when it loads, and its reply
    /// starts with the config — so this is the same answer the desktop itself
    /// would be given.
    pub fn config(&mut self) -> serde_json::Value {
        self.send(r#"{"type":"view.query"}"#);
        self.drain(Duration::from_millis(600))
            .into_iter()
            .find(|message| message["type"] == "config")
            .expect("no config event in the reply to view.query")
    }
}
