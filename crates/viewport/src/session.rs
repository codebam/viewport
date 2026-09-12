// SPDX-License-Identifier: GPL-3.0-or-later
//
// The saved layout. Ports src/session.c.
//
// Restarting the compositor kills every client with it, so nothing here
// preserves processes — it preserves *places*. The shell writes down its own
// tree with each window replaced by the application that was in it, and this
// hands the text back unread.

use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};

/// `$XDG_STATE_HOME/viewport/session.json`, falling back to
/// `~/.local/state/viewport/session.json`.
///
/// State rather than config: it is something the compositor writes, not
/// something the user edits.
pub fn path() -> Option<PathBuf> {
    let dir = match std::env::var("XDG_STATE_HOME") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir).join("viewport"),
        _ => PathBuf::from(std::env::var("HOME").ok()?).join(".local/state/viewport"),
    };

    if let Err(e) = create_state_dir(&dir) {
        tracing::error!(
            "cannot create {}: {e}; layout will not be remembered",
            dir.display()
        );
        return None;
    }
    Some(dir.join("session.json"))
}

/// The largest layout that will be written to disk.
///
/// The string is built by the shell from window titles and application names.
/// State that outgrows this is not a layout, and an unbounded page-supplied
/// string written to the compositor's state directory is a way to fill it up.
/// Refusing is deliberate where truncating is not: half a JSON document is
/// not a layout either, and keeping the previous one is the better failure.
const MAX_SESSION_BYTES: usize = 1 << 20;

/// Create the state directory if it is missing, and with no group or other
/// access when this call is the one that creates it. A directory that already
/// exists is left exactly as it is.
fn create_state_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(0o700);
    builder.create(dir)
}

pub fn save(state: &str) {
    if state.len() > MAX_SESSION_BYTES {
        tracing::warn!(
            "refusing to save a {} byte layout to session state (limit {MAX_SESSION_BYTES} bytes)",
            state.len()
        );
        return;
    }
    let Some(path) = path() else {
        return;
    };
    // Beside the file and renamed over it, rather than written in place.
    // `fs::write` truncates first, so a crash or a full disk between that and
    // the last byte destroys the last good layout in the act of saving a bad
    // one — which is exactly when the old layout is most wanted. A rename
    // within a directory is atomic, so `path` is always either the old layout
    // or the new one, never half of each.
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    let temp = path.with_file_name(name);

    let written = write_durable(&temp, state).and_then(|()| std::fs::rename(&temp, &path));
    if let Err(e) = written {
        tracing::error!("saving layout to {}: {e}", path.display());
        // Not left behind: the next save writes it again anyway, and a stale
        // half-layout sitting in the state directory is one more thing to
        // wonder about later.
        let _ = std::fs::remove_file(&temp);
    }
}

/// Write `state` to a file that did not exist, all the way out to the disk.
///
/// Fail-if-exists rather than truncate: a temp left by a save that died
/// mid-write is removed and replaced rather than appended into. `sync_all`
/// before the rename is what makes the rename worth anything — rename(2)
/// orders the name change against this process's writes, but without the sync
/// the data can still be in page cache when it lands, so a power cut after a
/// *successful* save could leave the layout file renaming onto nothing.
fn write_durable(path: &std::path::Path, state: &str) -> std::io::Result<()> {
    let _ = std::fs::remove_file(path);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path)?;
    use std::io::Write as _;
    file.write_all(state.as_bytes())?;
    file.sync_all()?;
    drop(file);
    // And the directory entry too, best effort: ext4 and its kin journal
    // metadata on their own schedule, and the parent's entry is the part that
    // says the new layout exists at all.
    if let Some(dir) = path.parent() {
        if let Ok(dir) = std::fs::File::open(dir) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

/// The stored layout, or an empty string when there is none.
///
/// A missing file is the ordinary first-run case, and said at debug for that
/// reason. Anything else — permissions, a directory that is not a directory,
/// a read that failed halfway — is said at warn with the error, because a
/// layout that exists but cannot be read is not the same as a layout that
/// does not, and the difference is invisible in a screenshot. Either way the
/// compositor comes up with an empty layout rather than failing: a broken
/// save is not worth a desktop that does not start.
/// Read at most [`MAX_SESSION_BYTES`] from a regular file, without following
/// a symlink at the final component.
///
/// The state directory is private, but the file inside it is still something
/// another process of the same user could replace with a symlink, a FIFO, or
/// an enormous file; a read that follows one is a way to block or exhaust the
/// compositor at startup. `load` only ever wants a layout, so anything that
/// is not one is treated as "no saved layout".
fn read_bounded(path: &Path) -> std::io::Result<String> {
    use std::io::Read as _;

    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    // O_NONBLOCK so a FIFO at this path returns from open() instead of
    // blocking a thread before the regular-file check can refuse it.
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    if metadata.len() > MAX_SESSION_BYTES as u64 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "larger than the session-state cap",
        ));
    }
    let mut text = String::new();
    file.take(MAX_SESSION_BYTES as u64 + 1)
        .read_to_string(&mut text)?;
    if text.len() > MAX_SESSION_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "larger than the session-state cap",
        ));
    }
    Ok(text)
}

pub fn load() -> String {
    let Some(path) = path() else {
        return String::new();
    };
    match read_bounded(&path) {
        Ok(contents) => {
            tracing::info!("restoring layout from {}", path.display());
            contents
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::debug!("no saved layout at {}", path.display());
            String::new()
        }
        Err(e) => {
            tracing::warn!("could not read the saved layout at {}: {e}", path.display());
            String::new()
        }
    }
}
