// SPDX-License-Identifier: GPL-3.0-or-later
//
// The wallpaper, from all three ways of setting one, against a real
// compositor.
//
// The unit tests cover the parsing: which mode names are accepted, what a path
// turns into. What they cannot cover is the part that decides whether anyone
// sees a picture — that the resolved URL reaches the shell in a `config`
// event, because the shell is what paints the desktop background and a
// wallpaper the compositor resolved and never announced is a wallpaper nobody
// has.
//
// Headless, so this needs no GPU, no display and no seat.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
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
    /// Headless, with whatever extra arguments the test is about.
    ///
    /// /tmp with the pid rather than CARGO_TARGET_TMPDIR, which is longer than
    /// `sockaddr_un.sun_path` allows.
    fn start(tag: &str, args: &[&str]) -> Self {
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
            .args(args)
            // Or a config file in the developer's own home decides what this
            // test sees.
            .env("XDG_CONFIG_HOME", "/nonexistent")
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr));

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

    /// Everything readable within a moment, as parsed JSON.
    fn drain(&mut self, within: Duration) -> Vec<serde_json::Value> {
        let deadline = Instant::now() + within;
        let mut messages = Vec::new();
        self.writer
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        while Instant::now() < deadline {
            let mut line = String::new();
            match self.reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    if let Ok(value) = serde_json::from_str(line.trim_end()) {
                        messages.push(value);
                    }
                }
                Err(_) => continue,
            }
        }
        messages
    }

    /// Ask for the config and hand back the one that comes past.
    ///
    /// `view.query` is the request a shell makes when it loads, and its reply
    /// starts with the config — so this is the same answer the desktop itself
    /// would be given.
    fn config(&mut self) -> serde_json::Value {
        self.send(r#"{"type":"view.query"}"#);
        self.drain(Duration::from_millis(600))
            .into_iter()
            .find(|message| message["type"] == "config")
            .expect("no config event in the reply to view.query")
    }
}

/// A file to point a wallpaper at. The contents are never decoded here —
/// nothing in this process draws — so what matters is only that it exists.
fn a_picture(name: &str) -> PathBuf {
    let path = PathBuf::from(format!("/tmp/viewport-test-{}-{name}", std::process::id()));
    std::fs::write(&path, b"not really a png").expect("write");
    path
}

/// `--wallpaper` reaches the shell as a URL it can load.
#[test]
fn a_wallpaper_from_the_command_line_reaches_the_shell() {
    let picture = a_picture("wall.png");
    let compositor = Compositor::start(
        "wallpaper-flag",
        &[
            "--wallpaper",
            &picture.to_string_lossy(),
            "--wallpaper-mode",
            "fit",
        ],
    );
    let mut client = compositor.connect();
    let config = client.config();

    // A URL and not the path that was typed: the page puts this straight in a
    // `url()`, and a bare path there loads nothing.
    assert_eq!(
        config["wallpaper"].as_str(),
        Some(format!("file://{}", picture.display()).as_str()),
        "{config}"
    );
    assert_eq!(config["wallpaper_mode"].as_str(), Some("fit"), "{config}");

    let _ = std::fs::remove_file(&picture);
}

/// A desktop nobody has given a picture says nothing about one.
///
/// Absent rather than null or an empty string, because the shell tests for the
/// key: an empty wallpaper that is *present* would take the gradient off and
/// leave the desktop black.
#[test]
fn no_wallpaper_is_no_key() {
    let compositor = Compositor::start("wallpaper-none", &[]);
    let mut client = compositor.connect();
    let config = client.config();
    assert!(config.get("wallpaper").is_none(), "{config}");
    assert!(config.get("wallpaper_mode").is_none(), "{config}");
}

/// `config.wallpaper` changes it at runtime, one field at a time, and takes it
/// away again.
#[test]
fn the_socket_sets_and_clears_the_wallpaper() {
    let picture = a_picture("socket-wall.png");
    let compositor = Compositor::start("wallpaper-socket", &[]);
    let mut client = compositor.connect();

    client.send(&format!(
        r#"{{"type":"config.wallpaper","path":"{}","mode":"tile"}}"#,
        picture.display()
    ));
    let config = client.config();
    assert_eq!(
        config["wallpaper"].as_str(),
        Some(format!("file://{}", picture.display()).as_str()),
        "{config}"
    );
    assert_eq!(config["wallpaper_mode"].as_str(), Some("tile"), "{config}");

    // The mode alone, which is what switching fitting while trying a picture
    // out looks like — the picture must survive it.
    client.send(r#"{"type":"config.wallpaper","mode":"center"}"#);
    let config = client.config();
    assert_eq!(
        config["wallpaper_mode"].as_str(),
        Some("center"),
        "{config}"
    );
    assert!(config["wallpaper"].is_string(), "{config}");

    // And the empty path, which is the only way back to the shell's own
    // background.
    client.send(r#"{"type":"config.wallpaper","path":""}"#);
    let config = client.config();
    assert!(config.get("wallpaper").is_none(), "{config}");

    let _ = std::fs::remove_file(&picture);
}

/// A colour or a gradient is a wallpaper too, and reaches the shell as written.
///
/// Nothing on the compositor's side resolves these — there is no file to find
/// — so the assertion is that they are passed through rather than run through
/// the path resolution and refused.
#[test]
fn a_colour_reaches_the_shell_as_written() {
    let compositor = Compositor::start("wallpaper-colour", &["--wallpaper", "#1a1b26"]);
    let mut client = compositor.connect();
    let config = client.config();
    assert_eq!(config["wallpaper"].as_str(), Some("#1a1b26"), "{config}");

    client.send(r#"{"type":"config.wallpaper","path":"linear-gradient(#1a1b26, #24283b)"}"#);
    let config = client.config();
    assert_eq!(
        config["wallpaper"].as_str(),
        Some("linear-gradient(#1a1b26, #24283b)"),
        "{config}"
    );
}

/// A picture that is not there is refused, and nothing else is applied with
/// it.
///
/// The failure this exists for: a wallpaper cycler pointed at a directory that
/// has been moved should not also lose the mode it was using, and a wallpaper
/// that silently does not appear is the whole reason the path is checked here
/// rather than in the page.
#[test]
fn a_wallpaper_that_is_not_there_is_refused() {
    let compositor = Compositor::start("wallpaper-missing", &[]);
    let mut client = compositor.connect();

    client.send(r#"{"type":"config.wallpaper","mode":"tile"}"#);
    let _ = client.config();

    client.send(r#"{"type":"config.wallpaper","path":"/nowhere/wall.png","mode":"fill"}"#);
    let messages = client.drain(Duration::from_millis(400));
    let error = messages
        .iter()
        .find(|message| message["type"] == "error")
        .expect("no error event");
    assert_eq!(error["context"], "config.wallpaper", "{error}");

    // The mode from the refused message was not applied either.
    let config = client.config();
    assert_eq!(config["wallpaper_mode"].as_str(), Some("tile"), "{config}");
    assert!(config.get("wallpaper").is_none(), "{config}");
}

/// An unknown fitting is refused rather than becoming `fill`.
#[test]
fn an_unknown_wallpaper_mode_is_refused() {
    let compositor = Compositor::start("wallpaper-mode", &[]);
    let mut client = compositor.connect();

    client.send(r#"{"type":"config.wallpaper","mode":"zoom"}"#);
    let messages = client.drain(Duration::from_millis(400));
    let error = messages
        .iter()
        .find(|message| message["type"] == "error")
        .expect("no error event");
    assert_eq!(error["context"], "config.wallpaper", "{error}");
    assert!(
        error["message"]
            .as_str()
            .unwrap_or_default()
            .contains("fill"),
        "the message should list the modes: {error}"
    );
}

/// The config file's own two keys, and the flag winning over them.
#[test]
fn the_config_file_sets_a_wallpaper_and_the_flag_wins() {
    let picture = a_picture("file-wall.png");
    let other = a_picture("flag-wall.png");
    let dir = PathBuf::from(format!(
        "/tmp/viewport-test-{}-wallpaper-config",
        std::process::id()
    ));
    std::fs::create_dir_all(dir.join("viewport")).expect("mkdir");
    let config_path = dir.join("viewport/config.json");
    std::fs::write(
        &config_path,
        format!(
            r#"{{"wallpaper":"{}","wallpaper_mode":"stretch"}}"#,
            picture.display()
        ),
    )
    .expect("write");

    {
        let compositor = Compositor::start(
            "wallpaper-config",
            &["--config", &config_path.to_string_lossy()],
        );
        let mut client = compositor.connect();
        let config = client.config();
        assert_eq!(
            config["wallpaper"].as_str(),
            Some(format!("file://{}", picture.display()).as_str()),
            "{config}"
        );
        assert_eq!(
            config["wallpaper_mode"].as_str(),
            Some("stretch"),
            "{config}"
        );
    }

    // The flag is the more deliberate of the two and is applied after the
    // file, which is the rule every other setting here follows.
    {
        let compositor = Compositor::start(
            "wallpaper-config-flag",
            &[
                "--config",
                &config_path.to_string_lossy(),
                "--wallpaper",
                &other.to_string_lossy(),
            ],
        );
        let mut client = compositor.connect();
        let config = client.config();
        assert_eq!(
            config["wallpaper"].as_str(),
            Some(format!("file://{}", other.display()).as_str()),
            "{config}"
        );
        // And the mode it did not name is still the file's.
        assert_eq!(
            config["wallpaper_mode"].as_str(),
            Some("stretch"),
            "{config}"
        );
    }

    let _ = std::fs::remove_file(&picture);
    let _ = std::fs::remove_file(&other);
    let _ = std::fs::remove_dir_all(&dir);
}
