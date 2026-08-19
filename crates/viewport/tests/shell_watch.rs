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

mod common;

use common::Compositor;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The line the compositor writes when the watch has been set up.
const WATCHING: &str = "for shell changes";
/// The line it writes when a change has settled and the page is being reloaded.
const RELOADING: &str = "the shell's files changed";

impl Compositor {
    /// Start one watching a shell directory of this test's own making.
    ///
    /// Its own directory rather than `data/shell`, because the test writes to
    /// what it watches and the repository's copy is not the test's to touch.
    /// The harness takes it away again when the compositor goes.
    ///
    /// Started ready rather than merely running: the socket appearing says
    /// nothing about the watch, and a test that saves a file before the inotify
    /// descriptor is in the event loop is testing the race instead.
    fn watching(tag: &str) -> Self {
        let shell = PathBuf::from(format!("/tmp/viewport-watch-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&shell);
        std::fs::create_dir_all(shell.join("vendor")).expect("a shell directory to watch");
        std::fs::write(shell.join("index.html"), "<!doctype html><title>t</title>")
            .expect("a page to load");

        Self::builder(tag)
            .prefix("viewport-watch")
            .args(["--watch-shell", "--url"])
            .arg(shell.join("index.html"))
            .owning(shell)
            .awaiting(WATCHING, Duration::from_secs(10))
            .start()
    }

    /// The shell directory this compositor is watching.
    fn shell(&self) -> &Path {
        self.directory()
    }

    /// Write a file the way an editor does: a temporary beside it, renamed
    /// over the top. `index.html` is never written to, so a watch looking for
    /// `IN_MODIFY` on it would see nothing at all.
    fn save(&self, name: &str, contents: &str) {
        let target = self.shell().join(name);
        let temporary = self.shell().join(format!(".{name}.tmp"));
        std::fs::write(&temporary, contents).expect("write");
        std::fs::rename(&temporary, &target).expect("rename");
    }
}

/// The whole point: edit a file, and the desktop reloads without being asked.
#[test]
fn saving_a_script_reloads_the_running_desktop() {
    let compositor = Compositor::watching("save");

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
    let compositor = Compositor::watching("subdirectory");

    std::fs::write(
        compositor.shell().join("vendor/gsap.min.js"),
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

/// And a subdirectory that was replaced, rather than written to, still counts.
///
/// `npm run vendor` removes `vendor/` and makes it again. A watch is on an
/// inode, so the one taken at startup is left pointing at a directory nothing
/// will ever write to again — and everything under `vendor/` stops reloading
/// for the rest of the session, silently, which is the one directory that
/// command exists to rewrite.
#[test]
fn a_subdirectory_that_was_recreated_is_watched_again() {
    let compositor = Compositor::watching("recreated");

    let vendor = compositor.shell().join("vendor");
    std::fs::remove_dir_all(&vendor).expect("remove");
    std::fs::create_dir(&vendor).expect("create");
    // The watch on the new directory is taken when the event announcing it is
    // read, which is after this call returns and before anything is written
    // inside it. A directory arriving empty is not itself worth a reload, and
    // this is also what keeps the assertion below honest.
    assert!(
        compositor
            .wait_for(RELOADING, Duration::from_millis(500))
            .is_none(),
        "an empty directory appearing reloaded the desktop:\n{}",
        compositor.log()
    );

    std::fs::write(vendor.join("gsap.min.js"), "window.gsap = {};\n").expect("write");

    assert!(
        compositor
            .wait_for(RELOADING, Duration::from_secs(5))
            .is_some(),
        "no reload after a change under a recreated vendor/:\n{}",
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
    let compositor = Compositor::watching("editor");

    std::fs::write(compositor.shell().join("4913"), "").expect("write");
    std::fs::write(compositor.shell().join(".state.js.swp"), "b0VIM").expect("write");
    std::fs::write(compositor.shell().join("index.html~"), "old").expect("write");

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
    let compositor = Compositor::watching("burst");

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

#[test]
fn saving_the_configuration_file_reloads_settings() {
    let dir = PathBuf::from(format!("/tmp/viewport-config-watch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let config = dir.join("config.json");
    std::fs::write(&config, "{\"gaps\":{\"inner\":10}}\n").expect("write");

    let compositor = Compositor::builder("config-watch")
        .prefix("viewport-watch")
        .args(["--watch-config", "--config"])
        .arg(&config)
        .owning(dir)
        .awaiting("for configuration changes", Duration::from_secs(10))
        .start();

    // Change the configuration file
    let temporary = config.with_extension("tmp");
    std::fs::write(&temporary, "{\"gaps\":{\"inner\":24}}\n").expect("write");
    std::fs::rename(&temporary, &config).expect("rename");

    assert!(
        compositor
            .wait_for(
                "the configuration file changed: reloading",
                Duration::from_secs(5)
            )
            .is_some(),
        "no reload after config change:\n{}",
        compositor.log()
    );
}
