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

mod common;

use common::Compositor;
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The direct children of a process that have exited and not been reaped.
///
/// Read from /proc rather than asked of the compositor, because the compositor
/// has no idea: a `Child` that is dropped without being waited for leaves a
/// process table entry nothing in this program will ever look at again.
fn zombie_children(parent: u32) -> Vec<u32> {
    let mut zombies = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return zombies;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        // The command is in parentheses and may contain spaces and parentheses
        // of its own, so the fields that matter are the ones after the last of
        // them: the state, and then the parent.
        let Some((_, rest)) = stat.rsplit_once(')') else {
            continue;
        };
        let fields: Vec<&str> = rest.split_whitespace().collect();
        if fields.first() == Some(&"Z") && fields.get(1) == Some(&parent.to_string().as_str()) {
            zombies.push(pid);
        }
    }
    zombies
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
    let compositor = Compositor::start_with_args("background", &[&flag]);

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
    let lines = client.drain_lines(Duration::from_secs(2));

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
    let compositor = Compositor::start_with_args("wallpaper", &[&flag]);
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
    let compositor = Compositor::start_with_args("per-output", &[&flag]);
    assert!(
        compositor.saw("background: started", Duration::from_secs(10)),
        "nothing was started for the first monitor:\n{}",
        compositor.log()
    );

    // A second screen, which the headless backend can plug in on request.
    let mut client = compositor.connect();
    client.send(r#"{"type":"output.test_add"}"#);
    let _ = client.drain_lines(Duration::from_millis(500));

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

    // And the process it killed is reaped. Nothing in the compositor handles
    // SIGCHLD, so a terminal killed and never waited for stays in the process
    // table for the rest of the session — one per monitor unplugged, which on
    // a laptop is one per time it is docked.
    let pid = compositor.child.id();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut zombies = zombie_children(pid);
    while !zombies.is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
        zombies = zombie_children(pid);
    }
    assert!(
        zombies.is_empty(),
        "the unplugged monitor's terminal was left as a zombie: {zombies:?}"
    );
}

/// The keyboard reaches it only when asked, and comes back.
///
/// The one deliberate way in. What this checks is that the request works and
/// that it is a toggle — the accidental routes are checked by
/// `the_wallpaper_terminal_is_not_a_window`, which is the assertion that it is
/// not a focusable thing in the first place.
#[test]
fn the_keyboard_can_be_handed_over_and_taken_back() {
    let Some(terminal) = terminal() else {
        eprintln!("skipped: no terminal emulator on PATH");
        return;
    };

    let flag = format!("--background-terminal={terminal}");
    let compositor = Compositor::start_with_args("focus", &[&flag]);
    if !compositor.saw("background: its toplevel arrived", Duration::from_secs(20)) {
        eprintln!("skipped: {terminal} never mapped under a headless compositor");
        return;
    }

    let mut client = compositor.connect();
    client.send(r#"{"type":"background.focus"}"#);
    assert!(
        compositor.saw(
            "the keyboard is on the wallpaper terminal",
            Duration::from_secs(5)
        ),
        "asking for it did not hand the keyboard over:\n{}",
        compositor.log()
    );

    client.send(r#"{"type":"background.focus"}"#);
    assert!(
        compositor.saw("the keyboard went back to view", Duration::from_secs(5)),
        "the same request did not give the keyboard back:\n{}",
        compositor.log()
    );
}

/// With the flag absent, nothing is started at all.
///
/// The switch is the presence of a command, and a desktop that has not asked
/// for one must not spawn a process behind it.
#[test]
fn no_flag_starts_no_terminal() {
    let compositor = Compositor::start_with_args("no-background", &[]);
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
