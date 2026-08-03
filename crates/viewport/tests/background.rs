// SPDX-License-Identifier: GPL-3.0-or-later
//
// A terminal drawn as the wallpaper, against a real compositor.
//
// The unit tests in `crate::background` cover which command gets run. What
// they cannot cover is the part that matters: that the client the compositor
// starts is adopted as the wallpaper and *not* as a window — because that is
// the whole of its isolation. A background terminal that reached the view
// registry would be focusable, and nothing about the way it is drawn would
// look any different.
//
// Headless, so this needs no GPU, no display and no seat. It needs a terminal
// emulator, and skips itself when there is none — the dev shell has `foot`,
// and a machine without one has nothing this could honestly assert.

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

    /// Wait for a line in the log, and say whether it turned up.
    fn saw(&self, needle: &str, within: Duration) -> bool {
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

    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// The Wayland display it created, from its own log.
    ///
    /// `WAYLAND_DISPLAY=<name>`, which is what the shell scripts in tests/
    /// grep for as well — one contract for both suites. Matching a bare word
    /// starting with `wayland-` would also hit Smithay's own `Created new
    /// socket name=Some("wayland-2")`.
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

    /// Everything readable within a moment, as lines.
    ///
    /// Time-bounded rather than counted: the point of most of these
    /// assertions is that a particular message never arrives, and there is no
    /// count that says "and nothing else".
    fn drain(&mut self, within: Duration) -> Vec<String> {
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
}

/// A terminal to use as the wallpaper, or nothing.
fn terminal() -> Option<&'static str> {
    first_on_path(&["foot", "alacritty", "kitty", "xterm"])
}

fn first_on_path(programs: &[&'static str]) -> Option<&'static str> {
    programs.iter().copied().find(|program| {
        std::env::var_os("PATH")
            .map(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
            .unwrap_or(false)
    })
}

/// The terminal is adopted as the wallpaper and never becomes a window.
///
/// `view.query` is the whole assertion: it replays one `view.added` per mapped
/// window, so a wallpaper that reached the view registry would be in the reply
/// — and would then be focusable, listed in the taskbar and offered as a
/// screen-share source, which is every guarantee in `crate::background` gone
/// at once.
#[test]
fn the_wallpaper_terminal_is_not_a_window() {
    let Some(terminal) = terminal() else {
        eprintln!("skipped: no terminal emulator on PATH");
        return;
    };

    let flag = format!("--background-terminal={terminal}");
    let compositor = Compositor::start("background", &[&flag]);

    assert!(
        compositor.saw("background: started", Duration::from_secs(10)),
        "the compositor never started it:\n{}",
        compositor.log()
    );

    // It has to get as far as a toplevel, or what follows proves nothing: a
    // terminal that never mapped is also not in `view.query`.
    if !compositor.saw("background: its toplevel arrived", Duration::from_secs(20)) {
        eprintln!(
            "skipped: {terminal} never mapped a window under a headless compositor:\n{}",
            compositor.log()
        );
        return;
    }

    let mut client = compositor.connect();
    client.send(r#"{"type":"view.query"}"#);
    let lines = client.drain(Duration::from_secs(2));

    assert!(
        !lines.iter().any(|line| line.contains("view.added")),
        "the wallpaper terminal was announced as a window: {lines:?}"
    );

    // And the compositor is still there. A wallpaper that takes the session
    // down with it is worse than no wallpaper.
    assert!(
        UnixStream::connect(&compositor.socket).is_ok(),
        "the compositor died:\n{}",
        compositor.log()
    );
}

/// A wallpaper program takes the position, and gets it back.
///
/// swaybg is a layer-shell client on the background layer, which is drawn over
/// everything the wallpaper terminal puts on the screen. Two things claiming
/// the same position is one of them painting frames nobody will ever see, so
/// the terminal closes — and starts again when the wallpaper program goes.
#[test]
fn a_wallpaper_program_takes_the_position() {
    let Some(terminal) = terminal() else {
        eprintln!("skipped: no terminal emulator on PATH");
        return;
    };
    let Some(wallpaper) = first_on_path(&["swaybg", "wbg", "hyprpaper"]) else {
        eprintln!("skipped: no wallpaper program on PATH");
        return;
    };

    let flag = format!("--background-terminal={terminal}");
    let compositor = Compositor::start("wallpaper", &[&flag]);
    if !compositor.saw("background: its toplevel arrived", Duration::from_secs(20)) {
        eprintln!("skipped: {terminal} never mapped under a headless compositor");
        return;
    }

    let Some(display) = compositor.wayland_display() else {
        panic!(
            "the compositor never announced a display:\n{}",
            compositor.log()
        );
    };

    // swaybg is the one with a colour-only mode, which needs no image file on
    // a machine that may have none. The others are started plain and skipped
    // if they will not run without arguments.
    let mut program = Command::new(wallpaper);
    if wallpaper == "swaybg" {
        program.args(["-c", "#112233"]);
    }
    let mut program = match program
        .env("WAYLAND_DISPLAY", &display)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            eprintln!("skipped: could not start {wallpaper}: {e}");
            return;
        }
    };

    assert!(
        compositor.saw(
            "background: a wallpaper program took the background layer",
            Duration::from_secs(15)
        ),
        "the terminal kept the wallpaper under {wallpaper}:\n{}",
        compositor.log()
    );

    // And the position comes back. A wallpaper program being killed and
    // restarted is what changing the wallpaper looks like from here, so the
    // terminal returning is not a nicety — without it, one `swaybg -c` from a
    // script would end the wallpaper terminal for the rest of the session.
    let _ = program.kill();
    let _ = program.wait();
    assert!(
        compositor.saw(
            "background: the wallpaper is free again",
            Duration::from_secs(15)
        ),
        "the terminal never came back after {wallpaper} went:\n{}",
        compositor.log()
    );
}

/// One terminal per monitor, and a monitor plugged in later gets its own.
///
/// The shell is one page across the whole layout and the wallpaper is not: a
/// terminal is a grid of cells and the cells have to land on a screen, so one
/// stretched across two monitors has half its columns on the other one and
/// every line cut down the middle by the gap between them.
#[test]
fn every_monitor_gets_a_terminal() {
    let Some(terminal) = terminal() else {
        eprintln!("skipped: no terminal emulator on PATH");
        return;
    };

    let flag = format!("--background-terminal={terminal}");
    let compositor = Compositor::start("per-output", &[&flag]);
    assert!(
        compositor.saw("background: started", Duration::from_secs(10)),
        "nothing was started for the first monitor:\n{}",
        compositor.log()
    );

    // A second screen, which the headless backend can plug in on request.
    let mut client = compositor.connect();
    client.send(r#"{"type":"output.test_add"}"#);
    let _ = client.drain(Duration::from_millis(500));

    let started: Vec<String> = {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let lines: Vec<String> = compositor
                .log()
                .lines()
                .filter(|line| line.contains("background: started"))
                .map(str::to_owned)
                .collect();
            if lines.len() >= 2 || Instant::now() > deadline {
                break lines;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    };

    assert!(
        started.len() >= 2,
        "the second monitor got no terminal of its own:\n{}",
        compositor.log()
    );
    // Each names the screen it belongs to, and they are different screens —
    // two terminals on one monitor would be two wallpapers in the same place.
    let names: Vec<&str> = started
        .iter()
        .filter_map(|line| line.split(" on ").nth(1))
        .filter_map(|rest| rest.split(" as pid").next())
        .collect();
    assert!(
        names.len() >= 2 && names[0] != names[1],
        "two terminals for the same monitor: {names:?}"
    );

    // And unplugging takes one with it, or a wallpaper outlives the screen it
    // was drawn on and paints frames for nothing.
    client.send(r#"{"type":"output.test_remove"}"#);
    assert!(
        compositor.saw(
            "went away, so its terminal did too",
            Duration::from_secs(10)
        ),
        "unplugging a monitor left its terminal running:\n{}",
        compositor.log()
    );
}

/// With the flag absent, nothing is started at all.
///
/// The switch is the presence of a command, and a desktop that has not asked
/// for one must not spawn a process behind it.
#[test]
fn no_flag_starts_no_terminal() {
    let compositor = Compositor::start("no-background", &[]);
    // Long enough for startup to have got there: the shell process is started
    // from the same place, so if that line is in the log the background one
    // would have been too.
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !compositor.log().contains("background: started"),
        "something was started behind the desktop:\n{}",
        compositor.log()
    );
}
