//! Reloading the shell's HTML, CSS and JavaScript while the session runs.
//!
//! The shell is a web page on disk, and editing a web page and looking at the
//! result is the whole loop of working on one. Restarting the compositor to see
//! a changed line is not that loop: it takes the windows with it, and on DRM it
//! takes the screen.
//!
//! Two ways in, both landing on [`ViewportState::reload_shells`]:
//!
//!   the keybinding    `reload` — `Mod4+Shift+c` by default — which is a person
//!                     saying "now", and needs nothing watched.
//!
//!   this module       an inotify watch over the directory the page was loaded
//!                     from, so saving a file *is* the reload. Off unless asked
//!                     for with `--watch-shell` or `VIEWPORT_WATCH_SHELL=1`,
//!                     because a reload throws the shell's state away and an
//!                     installed desktop's files never change under it anyway.
//!
//! Only a `file://` shell is watched. A page served over HTTP is a dev server's
//! job and there is nothing on this side to watch; the shipped shell in a
//! read-only store is watched happily and simply never fires.
//!
//! **A reload resets shell state.** Windows come back — the shell asks
//! `view.query` on load and the compositor replays every one of them — but
//! workspace assignments, the overview, and anything else the page was holding
//! do not. That is the same reload the keybinding has always done; watching
//! only changes what triggers it.

use std::path::{Path, PathBuf};

use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{Interest, Mode, PostAction};

use crate::state::ViewportState;

/// How long the files have to stop changing before the page is reloaded.
///
/// Not a delay for its own sake. An editor saving one file emits several
/// events — a temporary file created, written, renamed over the target, the old
/// one unlinked — and `npm run vendor` or a `git checkout` emits hundreds. Each
/// event re-arms this timer, so a burst of any size costs exactly one reload,
/// and it happens once the burst is over rather than in the middle of it, when
/// half the files on disk are the new ones and half are not.
const SETTLE: std::time::Duration = std::time::Duration::from_millis(200);

/// How deep to walk looking for directories to watch.
///
/// inotify watches one directory, not a tree, so every directory has to be
/// added by name. The shipped shell is one level deep (`vendor/`); the limit is
/// here so that pointing `--url` at a file in a home directory full of things
/// cannot turn into thousands of watches.
const DEPTH: usize = 4;

/// Extensions worth reloading for: what a browser actually fetches.
///
/// An allowlist rather than a list of things to ignore, because the things to
/// ignore are unbounded and editor-specific — vim writes a numeric probe file
/// and a `.swp`, Emacs writes `.#name` symlinks, an editor with autosave writes
/// something of its own — and every one of them would otherwise be a reload of
/// the desktop in the middle of a sentence.
const ASSETS: &[&str] = &[
    "html", "htm", "css", "js", "mjs", "json", "svg", "png", "jpg", "jpeg", "webp", "gif", "avif",
    "woff", "woff2", "ttf", "otf",
];

/// Whether a file the watch reported is one the page would load.
pub fn is_asset(name: &str) -> bool {
    // Rejected before the extension is looked at: `.index.html.swp` ends in
    // `swp` and would fail the check below anyway, but `.#shell.css` does not,
    // and neither does the `index.html~` some editors leave behind.
    if name.starts_with('.') || name.ends_with('~') {
        return false;
    }
    let Some((_, extension)) = name.rsplit_once('.') else {
        return false;
    };
    ASSETS
        .iter()
        .any(|known| known.eq_ignore_ascii_case(extension))
}

/// The directory a `file://` URL's page lives in, if it is one.
///
/// `None` for `http://` and anything else with a scheme: there is nothing local
/// to watch, and a dev server already reloads what it serves.
pub fn directory_of(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file://")?;
    // A query or fragment is part of the URL and not part of the path. `--url
    // …/index.html?debug=1` is a thing people type, and a path ending in `?…`
    // exists nowhere.
    let path = rest
        .split(['?', '#'])
        .next()
        .map(crate::config::percent_decode)?;
    let path = PathBuf::from(path);
    if path.is_dir() {
        return Some(path);
    }
    path.parent()
        .filter(|parent| parent.is_dir())
        .map(Path::to_path_buf)
}

/// `root` and every directory beneath it, to `DEPTH` levels.
///
/// Symlinks are not followed. A shell directory with a link back to its own
/// parent — or to `/nix/store` — would otherwise be walked until the depth
/// limit stopped it, having added a watch for every directory on the way.
fn tree(root: &Path, depth: usize, into: &mut Vec<PathBuf>) {
    into.push(root.to_path_buf());
    if depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        // `file_type` rather than `metadata`: the first describes the link and
        // the second describes what it points at.
        if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
            tree(&entry.path(), depth - 1, into);
        }
    }
}

/// Whether the session was asked to watch the shell.
pub fn wanted(args: &[String]) -> bool {
    if args.iter().any(|arg| arg == "--watch-shell") {
        return true;
    }
    // `1`, `true`, `yes` — and not `0`, which is how somebody turns it off in a
    // unit file that already sets it.
    match std::env::var("VIEWPORT_WATCH_SHELL") {
        Ok(value) => matches!(value.trim(), "1" | "true" | "yes" | "on"),
        Err(_) => false,
    }
}

impl ViewportState {
    /// Reload every shell that is running, whichever backend is drawing it.
    ///
    /// The in-process engine is reached directly; an out-of-process one is not
    /// here to be called, so the request travels the way everything else does
    /// and the shell process acts on it (`viewport_shell_bridge::is_reload`).
    ///
    /// Both, not one or the other: a build with `--features wpe` still runs an
    /// out-of-process shell when the backend was chosen at run time, and the
    /// two lists are empty when they do not apply.
    pub fn reload_shells(&mut self) {
        #[cfg(feature = "wpe")]
        for page in &self.shells {
            page.engine.reload();
        }
        if !self.shell_clients.is_empty() {
            self.notify(&viewport_ipc::Event::ShellReload);
        }
    }

    /// Watch the directories the shell was loaded from, and reload on a change.
    ///
    /// Called once, after the URL is settled and before anything is drawn.
    /// Failing here is logged and no more: a watch that could not be set up is
    /// a development convenience missing, not a desktop that cannot start.
    pub fn watch_shell_assets(&mut self) {
        use smithay::reexports::rustix::fs::inotify;

        // Every page that might run, whether or not it does. `plan_shells`
        // needs outputs to decide which of these gets which screen, and this
        // runs before there are any; the answer only ever narrows, so watching
        // both directories is right and costs one fd either way.
        let mut roots: Vec<PathBuf> = Vec::new();
        let candidates = [
            self.requested_url(),
            Some(crate::state::shipped_asset("shell/index.html")),
        ];
        for url in candidates.into_iter().flatten() {
            if let Some(directory) = directory_of(&url) {
                if !roots.contains(&directory) {
                    roots.push(directory);
                }
            }
        }
        if roots.is_empty() {
            tracing::info!(
                "--watch-shell: the shell is not a local page, so there is nothing to watch"
            );
            return;
        }

        let fd = match inotify::init(inotify::CreateFlags::NONBLOCK | inotify::CreateFlags::CLOEXEC)
        {
            Ok(fd) => fd,
            Err(e) => {
                tracing::warn!("--watch-shell: no inotify ({e}), so the shell is not watched");
                return;
            }
        };

        // What a save looks like from here. `CLOSE_WRITE` is a file written in
        // place; `MOVED_TO` is the rename an editor does instead, and is the
        // one that matters most — most editors write a temporary file and move
        // it over the target, so the target itself is never written to and
        // `MODIFY` never fires for it. `CREATE` and `DELETE` catch a file
        // appearing or going away, which is `git checkout` and `npm run
        // vendor`.
        let watching = inotify::WatchFlags::CLOSE_WRITE
            | inotify::WatchFlags::MOVED_TO
            | inotify::WatchFlags::MOVED_FROM
            | inotify::WatchFlags::CREATE
            | inotify::WatchFlags::DELETE;

        let mut directories: Vec<PathBuf> = Vec::new();
        for root in &roots {
            tree(root, DEPTH, &mut directories);
        }
        // Kept by descriptor rather than counted, because a directory that
        // appears later has to be watched too and the event that says so names
        // it relative to the directory it appeared in. See the closure.
        let mut watches: std::collections::HashMap<i32, PathBuf> = std::collections::HashMap::new();
        for directory in &directories {
            match inotify::add_watch(&fd, directory, watching) {
                Ok(wd) => {
                    watches.insert(wd, directory.clone());
                }
                Err(e) => tracing::warn!("--watch-shell: {}: {e}", directory.display()),
            }
        }
        let watched = watches.len();
        if watched == 0 {
            tracing::warn!("--watch-shell: nothing could be watched, so the shell is not watched");
            return;
        }

        // The debounce, on a timerfd for the reason every other clock here is
        // on one: under the web engine GLib owns the blocking poll and calloop's
        // own timer wheel is invisible to it, so a wheel entry would not fire
        // until something else woke the loop — which on a desktop nobody is
        // touching is exactly never. See [`ViewportState::create_tick`].
        self.shell_reload_timer = self.create_tick("shell watch", Self::shell_assets_settled);

        if let Err(e) = self.loop_handle.insert_source(
            Generic::new(fd, Interest::READ, Mode::Level),
            move |_, fd, state: &mut Self| {
                // Drained to the end, or a level-triggered source reports the
                // same events for ever and the loop never sleeps again.
                let mut buffer = [std::mem::MaybeUninit::<u8>::uninit(); 4096];
                let mut reader = inotify::Reader::new(&*fd, &mut buffer);
                let mut interesting = false;
                // Directories that have just appeared, and descriptors that
                // have just stopped meaning anything. Collected here and acted
                // on below rather than in the middle of a drain that has to
                // finish.
                let mut appeared: Vec<PathBuf> = Vec::new();
                let mut gone: Vec<i32> = Vec::new();
                loop {
                    match reader.next() {
                        Ok(event) => {
                            let name = event
                                .file_name()
                                .map(|name| name.to_string_lossy().into_owned())
                                .unwrap_or_default();
                            let flags = event.events();
                            // A watch is one directory, and the set was walked
                            // once at startup. `npm run vendor` removes
                            // `vendor/` and makes it again, which leaves the
                            // watch on an inode nothing writes to ever again —
                            // so nothing under `vendor/` reloads for the rest
                            // of the session, which is the one directory the
                            // command exists to rewrite.
                            if flags.contains(inotify::ReadFlags::ISDIR)
                                && flags.intersects(
                                    inotify::ReadFlags::CREATE | inotify::ReadFlags::MOVED_TO,
                                )
                            {
                                if let Some(parent) = watches.get(&event.wd()) {
                                    appeared.push(parent.join(&name));
                                }
                            }
                            // The kernel says a watch is spent when what it
                            // watched went away. Dropping it keeps the map the
                            // size of the tree rather than of the session.
                            if flags.contains(inotify::ReadFlags::IGNORED) {
                                gone.push(event.wd());
                            }
                            if is_asset(&name) {
                                interesting = true;
                            }
                        }
                        // Everything pending has been read.
                        Err(smithay::reexports::rustix::io::Errno::WOULDBLOCK) => break,
                        Err(e) => {
                            tracing::warn!("shell watch: {e}");
                            break;
                        }
                    }
                }
                for wd in gone {
                    watches.remove(&wd);
                }
                // Walked rather than added on its own: a directory that arrived
                // as a whole tree — a rename over the top, which is how a
                // vendor step lands — has everything under it appear without a
                // further event, and nothing beneath the top level would be
                // watched.
                for directory in appeared {
                    let mut fresh = Vec::new();
                    tree(&directory, DEPTH, &mut fresh);
                    for directory in fresh {
                        // Anything the page loads that is already inside it
                        // arrived before there was a watch to see it arrive.
                        if !interesting {
                            interesting = std::fs::read_dir(&directory)
                                .map(|entries| {
                                    entries
                                        .flatten()
                                        .any(|entry| is_asset(&entry.file_name().to_string_lossy()))
                                })
                                .unwrap_or(false);
                        }
                        match inotify::add_watch(&*fd, &directory, watching) {
                            // Re-adding an existing watch returns the
                            // descriptor it already had, so a directory that
                            // was somehow watched twice is one entry either
                            // way.
                            Ok(wd) => {
                                watches.insert(wd, directory);
                            }
                            Err(e) => {
                                tracing::warn!("shell watch: {}: {e}", directory.display())
                            }
                        }
                    }
                }
                if interesting {
                    state.shell_assets_changed();
                }
                Ok(PostAction::Continue)
            },
        ) {
            tracing::warn!("--watch-shell: could not watch the shell ({e})");
            return;
        }

        tracing::info!(
            "watching {watched} director{} for shell changes; a save reloads the desktop",
            if watched == 1 { "y" } else { "ies" }
        );
    }

    /// Something under the shell's directory changed: reload once it stops.
    ///
    /// Re-arming an armed timer replaces it rather than adding to it, so this
    /// is the whole of the debounce — the reload happens `SETTLE` after the
    /// *last* change, however many arrived.
    fn shell_assets_changed(&mut self) {
        if Self::arm_tick("shell watch", self.shell_reload_timer.as_ref(), SETTLE) {
            return;
        }

        // No timerfd. calloop's own timer still works whenever calloop is the
        // one waiting, which is every backend except the web engine's, and a
        // late reload is a better outcome than none. The flag is what keeps a
        // burst of events from queueing a timer per file.
        if self.shell_reload_pending {
            return;
        }
        self.shell_reload_pending = true;
        let timer = smithay::reexports::calloop::timer::Timer::from_duration(SETTLE);
        if let Err(e) = self
            .loop_handle
            .insert_source(timer, move |_, _, state: &mut Self| {
                state.shell_assets_settled();
                smithay::reexports::calloop::timer::TimeoutAction::Drop
            })
        {
            tracing::warn!("shell watch: {e}");
            self.shell_reload_pending = false;
        }
    }

    /// The files have stopped changing. Reload.
    fn shell_assets_settled(&mut self) {
        self.shell_reload_pending = false;
        tracing::info!("the shell's files changed: reloading");
        self.reload_shells();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_what_the_page_loads_is_worth_reloading_for() {
        for name in [
            "index.html",
            "shell.css",
            "state.js",
            "gsap.min.js",
            "icon.svg",
            "Inter.woff2",
        ] {
            assert!(is_asset(name), "{name}");
        }
    }

    #[test]
    fn an_editor_writing_beside_a_file_does_not_reload_the_desktop() {
        // vim's swap file and its numeric probe, Emacs' lock symlink, the
        // backup a save leaves behind, and this repository's own notes.
        for name in [
            ".index.html.swp",
            "4913",
            ".#shell.css",
            "index.html~",
            "shell.md",
            "README",
        ] {
            assert!(!is_asset(name), "{name}");
        }
    }

    #[test]
    fn a_page_served_over_http_has_nothing_local_to_watch() {
        assert_eq!(directory_of("http://localhost:8000/index.html"), None);
        assert_eq!(directory_of("https://example.com/"), None);
    }

    #[test]
    fn a_file_url_is_watched_by_the_directory_holding_it() {
        let dir = std::env::temp_dir().join("viewport-watch-directory-of");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("index.html");
        std::fs::write(&file, "<!doctype html>").expect("a file to point at");

        let url = format!("file://{}", file.display());
        assert_eq!(directory_of(&url).as_deref(), Some(dir.as_path()));

        // A URL naming the directory itself is already the answer.
        let url = format!("file://{}", dir.display());
        assert_eq!(directory_of(&url).as_deref(), Some(dir.as_path()));

        // And a query string is not part of the path.
        let url = format!("file://{}?debug=1", file.display());
        assert_eq!(directory_of(&url).as_deref(), Some(dir.as_path()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_encoded_space_names_the_directory_it_stands_for() {
        let dir = std::env::temp_dir().join("viewport watch encoded");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("index.html");
        std::fs::write(&file, "<!doctype html>").expect("a file to point at");

        let url = format!(
            "file://{}/index.html",
            dir.display().to_string().replace(' ', "%20")
        );
        assert_eq!(directory_of(&url).as_deref(), Some(dir.as_path()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_walk_finds_subdirectories_and_stops_at_the_limit() {
        let root = std::env::temp_dir().join("viewport-watch-tree");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("vendor/deep/deeper")).expect("a tree to walk");

        let mut found = Vec::new();
        tree(&root, 1, &mut found);
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found.contains(&root.join("vendor")), "{found:?}");

        let mut found = Vec::new();
        tree(&root, DEPTH, &mut found);
        assert_eq!(found.len(), 4, "{found:?}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn watching_is_off_unless_it_is_asked_for() {
        assert!(!wanted(&["viewport".to_owned()]));
        assert!(wanted(&["viewport".to_owned(), "--watch-shell".to_owned()]));
    }
}
