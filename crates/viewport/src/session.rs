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
        _ => PathBuf::from(std::env::var("HOME").ok()?)
            .join(".local/state/viewport"),
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
    if let Err(e) = std::fs::write(&path, state) {
        tracing::error!("saving layout to {}: {e}", path.display());
    }
}

/// The stored layout, or an empty string when there is none.
///
/// A missing file is the ordinary first-run case, not an error.
pub fn load() -> String {
    let Some(path) = path() else {
        return String::new();
    };
    match std::fs::read_to_string(&path) {
        Ok(contents) => {
            tracing::info!("restoring layout from {}", path.display());
            contents
        }
        Err(_) => {
            tracing::debug!("no saved layout at {}", path.display());
            String::new()
        }
    }
}
