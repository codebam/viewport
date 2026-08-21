// SPDX-License-Identifier: GPL-3.0-or-later
//
// The saved layout. Ports src/session.c.
//
// Restarting the compositor kills every client with it, so nothing here
// preserves processes — it preserves *places*. The shell writes down its own
// tree with each window replaced by the application that was in it, and this
// hands the text back unread.

use std::path::PathBuf;

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

    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::error!(
            "cannot create {}: {e}; layout will not be remembered",
            dir.display()
        );
        return None;
    }
    Some(dir.join("session.json"))
}

pub fn save(state: &str) {
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

    let written = std::fs::write(&temp, state).and_then(|()| std::fs::rename(&temp, &path));
    if let Err(e) = written {
        tracing::error!("saving layout to {}: {e}", path.display());
        // Not left behind: the next save writes it again anyway, and a stale
        // half-layout sitting in the state directory is one more thing to
        // wonder about later.
        let _ = std::fs::remove_file(&temp);
    }
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
pub fn load() -> String {
    let Some(path) = path() else {
        return String::new();
    };
    match std::fs::read_to_string(&path) {
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
