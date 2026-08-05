// SPDX-License-Identifier: GPL-3.0-or-later
//
// That saving a file under the shell's directory reloads the running desktop.
//
// A real compositor, headless, watching a directory this test owns. What it
// covers is the seam the unit tests in src/shell_watch.rs cannot: that the
// inotify descriptor is actually in the event loop, that the debounce timer
// actually fires on a session where nothing else is happening, and that an
// editor's scratch files do not drag the desktop through a reload every time
// somebody opens a file.
//
// The reload itself lands on no shell here — a headless test run has no shell
// process — and that is fine: what is being tested is everything up to and
// including the decision, which is the part that was missing.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// The line the compositor writes when the watch has been set up.
const WATCHING: &str = "for shell changes";
/// The line it writes when a change has settled and the page is being reloaded.
const RELOADING: &str = "the shell's files changed";

struct Compositor {
    child: Child,
    log: PathBuf,
    socket: PathBuf,
    shell: PathBuf,
}

impl Drop for Compositor {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::remove_file(&self.log);
        let _ = std::fs::remove_dir_all(&self.shell);
    }
}

impl Compositor {
    /// Start one watching a shell directory of this test's own making.
    ///
    /// Its own directory rather than `data/shell`, because the test writes to
    /// what it watches and the repository's copy is not the test's to touch.
    fn start(tag: &str) -> Self {
        let shell = PathBuf::from(format!("/tmp/viewport-watch-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&shell);
        std::fs::create_dir_all(shell.join("vendor")).expect("a shell directory to watch");
        std::fs::write(shell.join("index.html"), "<!doctype html><title>t</title>")
            .expect("a page to load");

        let socket = PathBuf::from(format!(
            "/tmp/viewport-watch-{}-{tag}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&socket);

        let log = PathBuf::from(format!(
            "/tmp/viewport-watch-{}-{tag}.log",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&log);
        let stderr = std::fs::File::create(&log).expect("could not create the log");

        let child = Command::new(env!("CARGO_BIN_EXE_viewport"))
            .args(["--headless", "--watch-shell", "--socket"])
            .arg(&socket)
            .arg("--url")
            .arg(shell.join("index.html"))
            // Otherwise a config file in the developer's own home decides what
            // this test sees.
            .env("XDG_CONFIG_HOME", "/nonexistent")
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr))
            .spawn()
            .expect("could not start the compositor");

        let compositor = Self {
            child,
            log,
            socket,
            shell,
        };
        compositor
            .wait_for(WATCHING, Duration::from_secs(10))
            .expect("the compositor never said it was watching anything");
        compositor
    }

    /// Wait for a line in the log, and say how long it took.
    fn wait_for(&self, needle: &str, patience: Duration) -> Option<Duration> {
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

    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// Write a file the way an editor does: a temporary beside it, renamed
    /// over the top. `index.html` is never written to, so a watch looking for
    /// `IN_MODIFY` on it would see nothing at all.
    fn save(&self, name: &str, contents: &str) {
        let target = self.shell.join(name);
        let temporary = self.shell.join(format!(".{name}.tmp"));
        std::fs::write(&temporary, contents).expect("write");
        std::fs::rename(&temporary, &target).expect("rename");
    }
}

/// The whole point: edit a file, and the desktop reloads without being asked.
#[test]
fn saving_a_script_reloads_the_running_desktop() {
    let compositor = Compositor::start("save");

    compositor.save("state.js", "let focusedId = 0;\n");

    let waited = compositor
        .wait_for(RELOADING, Duration::from_secs(5))
        .unwrap_or_else(|| panic!("no reload after a save:\n{}", compositor.log()));

    // The debounce is 200ms and nothing else is happening, so this should be
    // prompt. Loose enough for a loaded CI machine, tight enough that a reload
    // arriving only because something else woke the loop would fail.
    assert!(
        waited < Duration::from_secs(3),
        "the reload took {waited:?}, which is not a save-and-look loop"
    );
}

/// A file in a subdirectory counts. `vendor/` is where GSAP lives, and
/// `npm run vendor` rewriting it is exactly a change worth reloading for.
#[test]
fn a_subdirectory_is_watched_too() {
    let compositor = Compositor::start("subdirectory");

    std::fs::write(
        compositor.shell.join("vendor/gsap.min.js"),
        "window.gsap = {};\n",
    )
    .expect("write");

    assert!(
        compositor
            .wait_for(RELOADING, Duration::from_secs(5))
            .is_some(),
        "no reload after a change under vendor/:\n{}",
        compositor.log()
    );
}

/// And opening a file in vim does not.
///
/// vim creates a numeric probe file to test whether the directory is writable
/// and a `.swp` beside the file it is editing, both before a single character
/// has been typed. Reloading for either is a desktop that resets itself when
/// somebody starts reading their own shell.
#[test]
fn an_editor_opening_a_file_does_not_reload_anything() {
    let compositor = Compositor::start("editor");

    std::fs::write(compositor.shell.join("4913"), "").expect("write");
    std::fs::write(compositor.shell.join(".state.js.swp"), "b0VIM").expect("write");
    std::fs::write(compositor.shell.join("index.html~"), "old").expect("write");

    // Long enough to be past the debounce several times over.
    assert!(
        compositor
            .wait_for(RELOADING, Duration::from_millis(1500))
            .is_none(),
        "an editor's scratch files reloaded the desktop:\n{}",
        compositor.log()
    );

    // And the watch is still live afterwards, rather than having been spent.
    compositor.save("shell.css", "body { margin: 0 }\n");
    assert!(
        compositor
            .wait_for(RELOADING, Duration::from_secs(5))
            .is_some(),
        "no reload after a real save:\n{}",
        compositor.log()
    );
}

/// A burst is one reload, not one per file. `git checkout` of a branch that
/// touched every script would otherwise be twenty reloads in a row, most of
/// them of a directory that is half one revision and half another.
#[test]
fn a_burst_of_changes_reloads_once() {
    let compositor = Compositor::start("burst");

    for name in [
        "state.js",
        "motion.js",
        "tiling.js",
        "scrolling.js",
        "windows.js",
        "bar.js",
        "shell.css",
    ] {
        compositor.save(name, "// rewritten\n");
    }

    assert!(
        compositor
            .wait_for(RELOADING, Duration::from_secs(5))
            .is_some(),
        "no reload after a burst:\n{}",
        compositor.log()
    );
    // Settle, so a second reload would have been written by now.
    std::thread::sleep(Duration::from_millis(750));

    let reloads = compositor.log().matches(RELOADING).count();
    assert_eq!(
        reloads,
        1,
        "{reloads} reloads for one burst:\n{}",
        compositor.log()
    );
}
