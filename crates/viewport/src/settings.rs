// SPDX-License-Identifier: GPL-3.0-or-later
//
// The runtime settings overlay: how a change made from the settings panel
// survives the next start.
//
// The two tiers docs/configuration.md opens with are a bootstrap file that is
// read before any web content loads and a set of runtime setters that
// deliberately do not touch the disk. That split is right and it leaves one
// question open, which the settings panel is the first thing to actually ask:
// where does a change go when somebody means it? A panel whose every setting
// is forgotten at the next restart is not a settings panel, it is a preview.
//
// There were three answers and this is the one taken.
//
// **Not writing back into the config file.** That file is hand-written JSONC:
// comments, blank lines, the order somebody chose, keys grouped the way they
// think about them. Nothing here can round-trip that — serde_json parses to a
// value and prints a value, so a save would hand back a machine's idea of the
// same settings with every comment gone. Losing a person's notes about their
// own desktop as a side effect of dragging a gap slider is not a trade a
// settings panel gets to make on their behalf. It is also the file the
// compositor needs in order to start on a broken display, which is the whole
// argument for the bootstrap tier existing, and the last file that should be
// rewritten by the part of the desktop most likely to be wrong.
//
// **Not saving on every change either.** The runtime setters are what a
// wallpaper cycler uses — a new picture every hour, one message and no config
// reload — and a setter that wrote the disk would turn that into a file
// rewritten twenty-four times a day. It would also mean a value tried out and
// disliked is already persisted by the time it is disliked.
//
// **So: an overlay file, written when asked.** `settings.json`, beside
// `config.json`, holding only the keys the panel has actually set, applied on
// top of the config file at startup and at every reload. `config.save` is what
// writes it, and nothing else does. The three properties that buys:
//
//   - the config file is never touched, so nobody's comments die;
//   - the layering is the one that already exists — the overlay is parsed as a
//     `config::File` and handed to `apply_config`, which has always meant "only
//     the keys present", so there is no second code path deciding what wins;
//   - a config file edited by hand still loses to the overlay, which is the
//     right way round for a panel: the last thing you did in the UI is what
//     you meant. Deleting `settings.json` puts the file back in charge, which
//     is the escape hatch, and is documented as one.
//
// What this file owns is the writing. The reading is `config::load` on a
// second path, because the overlay is a config file with fewer keys in it and
// giving it a parser of its own would be a second schema to keep in step.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

/// The overlay file that goes beside a config file.
///
/// Beside it rather than in a directory of its own so that `--config` moves
/// both together: a session started against a config file in a test's temp
/// directory must not write its settings into the developer's real one.
pub fn path(config_path: &Path) -> PathBuf {
    config_path.with_file_name("settings.json")
}

/// What a save writes.
///
/// Serialize only, and every field skipped when it is absent: the file is
/// meant to hold what has been set and nothing else, so that reading it says
/// what the panel changed rather than restating the defaults. It is parsed
/// back as a [`crate::config::File`], which is a superset of this — see the
/// round-trip test at the bottom, which is what stops a key here from drifting
/// out of the shape the config file reader expects.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Overlay {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dark_mode: Option<bool>,

    /// The URL the compositor resolved, not the path somebody typed.
    ///
    /// `wallpaper_value` accepts a `file://` URL and checks the file is still
    /// there, so what it produced goes back in unchanged and comes back out
    /// the same — and a picture deleted between sessions is a line in the log
    /// rather than a desktop that silently lost its background.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wallpaper: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub wallpaper_mode: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub gaps: Option<viewport_ipc::event::Gaps>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub border: Option<viewport_ipc::event::Border>,

    /// The monitors, by connector name.
    ///
    /// A `BTreeMap` rather than a `HashMap` so the file comes out in the same
    /// order every time. Nobody diffs this by hand often, but a file that
    /// reshuffles itself on every save is one that looks like it changed when
    /// it did not, and version control notices.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub outputs: BTreeMap<String, OutputOverlay>,
}

/// One monitor's arrangement, spelled the way the `outputs` block spells it.
///
/// A mirror of the fields of [`crate::config::OutputConfig`] that a panel can
/// set, written out rather than reusing that type because that one is the
/// reader's and gains keys — `max_refresh`, `hdr` — that are questions rather
/// than states. Saving a resolved 2560x1440@240 as `max_refresh: true` would
/// be writing down the question instead of the answer, and the answer changes
/// when the monitor does.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct OutputOverlay {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// `WIDTHxHEIGHT@RATE`, as `config::parse_mode` reads it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scale: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transform: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub x: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub y: Option<i32>,
}

/// Write the overlay, atomically.
///
/// Through a temporary file in the same directory and a rename, because the
/// alternative is a truncated `settings.json` on the one occasion it matters
/// most: the compositor being killed, or the machine losing power, in the
/// middle of the write. A half-written overlay is not a lost setting, it is a
/// session that refuses to start with the settings it was given — the reader
/// treats a parse error as fatal for `--config` and as "keep what we have" on
/// reload, and neither is a good thing to wake up to.
///
/// The parent directory is created if it is not there, which is the ordinary
/// case for a desktop nobody has configured: `~/.config/viewport` does not
/// exist until something puts a file in it, and refusing to save because the
/// user never hand-wrote a config file would be refusing the exact case the
/// panel exists for.
///
/// An overlay with nothing in it is written as `{}` rather than skipped: that
/// is how "the panel was used to put everything back to the config file's
/// values" is recorded, and an overlay that layers nothing is exactly what
/// that means.
pub fn save(path: &Path, overlay: &Overlay) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| anyhow::anyhow!("{}: {e}", dir.display()))?;
    }

    // Pretty, with a trailing newline. This is a file a person will open to
    // find out what the panel decided, and to delete a line out of when they
    // want the config file to win again; one long line would make both of
    // those worse for no gain anybody can measure on a file this size.
    let mut text = serde_json::to_string_pretty(overlay)
        .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
    text.push('\n');

    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, text)
        .map_err(|e| anyhow::anyhow!("{}: {e}", temporary.display()))?;
    std::fs::rename(&temporary, path).map_err(|e| {
        // The temporary is cleaned up here rather than left behind: a rename
        // that failed is usually a read-only or full filesystem, and leaving a
        // `settings.json.tmp` next to the config file is a puzzle for whoever
        // finds it later.
        let _ = std::fs::remove_file(&temporary);
        anyhow::anyhow!("{}: {e}", path.display())
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full() -> Overlay {
        let mut outputs = BTreeMap::new();
        outputs.insert(
            "DP-1".to_owned(),
            OutputOverlay {
                enabled: Some(true),
                mode: Some("2560x1440@144".to_owned()),
                scale: Some(1.25),
                transform: Some("90".to_owned()),
                x: Some(0),
                y: Some(0),
            },
        );
        Overlay {
            dark_mode: Some(false),
            wallpaper: Some("file:///pic/wall.png".to_owned()),
            wallpaper_mode: Some("tile".to_owned()),
            gaps: Some(viewport_ipc::event::Gaps {
                inner: Some(12),
                outer: Some(4),
                smart: Some(true),
            }),
            border: Some(viewport_ipc::event::Border {
                radius: Some(10),
                width: Some(3),
                smart: Some(false),
            }),
            outputs,
        }
    }

    /// The whole reason this type is allowed to exist separately from
    /// `config::File`: what it writes has to be something that reader
    /// understands, key for key. A field renamed on one side and not the other
    /// would otherwise be a setting that saves, reloads as absent, and is
    /// reported by nothing at all.
    #[test]
    fn every_key_written_is_a_key_the_config_reader_reads() {
        let text = serde_json::to_string(&full()).expect("should serialise");
        let file: crate::config::File = serde_json::from_str(&text).expect("should parse");

        assert_eq!(file.dark_mode, Some(false));
        assert_eq!(file.wallpaper.as_deref(), Some("file:///pic/wall.png"));
        assert_eq!(file.wallpaper_mode.as_deref(), Some("tile"));
        assert_eq!(file.gaps.inner, Some(12));
        assert_eq!(file.gaps.outer, Some(4));
        assert_eq!(file.gaps.smart, Some(true));
        assert_eq!(file.border.radius, Some(10));
        assert_eq!(file.border.width, Some(3));
        assert_eq!(file.border.smart, Some(false));

        let output = file
            .outputs
            .get("DP-1")
            .expect("the monitor should be there");
        assert_eq!(output.enabled, Some(true));
        assert_eq!(output.mode.as_deref(), Some("2560x1440@144"));
        assert_eq!(output.scale, Some(1.25));
        assert_eq!(output.transform.as_deref(), Some("90"));
        assert_eq!(output.x, Some(0));
        assert_eq!(output.y, Some(0));
    }

    /// An overlay is what the panel set, not a restatement of the defaults —
    /// otherwise every save would freeze the shipped defaults into the file
    /// and a later release could never change one.
    #[test]
    fn nothing_set_writes_nothing() {
        assert_eq!(
            serde_json::to_string(&Overlay::default()).expect("should serialise"),
            "{}"
        );
        assert_ne!(
            serde_json::to_string(&full()).expect("should serialise"),
            "{}"
        );
    }

    /// Beside the config file, whichever one that is — so `--config` in a
    /// test's temp directory cannot write into the developer's real settings.
    #[test]
    fn the_overlay_follows_the_config_file_it_belongs_to() {
        assert_eq!(
            path(Path::new("/tmp/x/viewport/config.json")),
            PathBuf::from("/tmp/x/viewport/settings.json")
        );
    }

    #[test]
    fn a_save_creates_the_directory_and_replaces_what_was_there() {
        let dir = std::env::temp_dir().join(format!("viewport-settings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("viewport/settings.json");

        save(&file, &full()).expect("should write");
        let first: crate::config::File =
            serde_json::from_str(&std::fs::read_to_string(&file).expect("should read"))
                .expect("should parse");
        assert_eq!(first.dark_mode, Some(false));

        // Replaced whole rather than merged: the overlay is the panel's
        // current answer, and a save that only ever added keys could never
        // take one back out.
        save(&file, &Overlay::default()).expect("should write again");
        assert_eq!(
            std::fs::read_to_string(&file).expect("should read").trim(),
            "{}"
        );
        // And nothing left behind from the atomic write.
        assert!(!file.with_extension("json.tmp").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
