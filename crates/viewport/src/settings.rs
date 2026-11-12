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
//   - the layering is the one that already exists — the overlay's keys are
//     laid over the parsed config file by [`apply`] before the one
//     `apply_config`, so only the keys present take effect and there is no
//     second code path deciding what wins;
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

use serde::{Deserialize, Serialize};

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
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
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

    /// Spelled the way the `cursor` block spells it, and parsed back the
    /// same way — see the round-trip test below.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<viewport_ipc::event::Cursor>,

    /// Spelled the way the `motion` block spells it, and parsed back the
    /// same way.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub motion: Option<viewport_ipc::event::Motion>,

    /// Spelled the way the `group` block spells it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<viewport_ipc::event::Group>,

    /// Spelled the way the `clock` block spells it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clock: Option<viewport_ipc::event::Clock>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub bar: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub bar_blur: Option<bool>,

    /// Spelled the way `bar_widgets` spells it — the same tagged objects
    /// the file takes. The `ai` widget's credential env names do not
    /// survive this trip: what was set lives in the compositor's copy of
    /// the list, and only the provider reaches the shell.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bar_widgets: Option<Vec<viewport_ipc::event::BarWidget>>,

    /// Spelled the way `bar_items` spells it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bar_items: Option<Vec<viewport_ipc::event::BarItem>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub logo: Option<bool>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub tutorial: Option<bool>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub tiling_mode: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub layout: Option<String>,

    /// The file's map form — name to the URL already resolved — not the
    /// shell's list, so what is written is what the config reader reads.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layout_extensions: Option<BTreeMap<String, String>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub focus_crosses_outputs: Option<bool>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub osk: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub tray: Option<bool>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub clipboard_history: Option<usize>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon_theme: Option<String>,

    /// The file's `theme` value, as parsed JSON.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme: Option<serde_json::Value>,

    /// The file's `rules` value, as parsed JSON.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rules: Option<serde_json::Value>,

    /// The file's `layer_rules` value, as parsed JSON.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layer_rules: Option<serde_json::Value>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub decorations: Option<String>,

    /// The file's `workspaces` value — number to rule — as parsed JSON, so
    /// it parses back as the file's own keys.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspaces: Option<serde_json::Value>,

    /// Spelled the way the `opacity` block spells it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub opacity: Option<viewport_ipc::event::Opacity>,

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
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mirror: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vrr: Option<viewport_ipc::event::VrrMode>,
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
        // A single-component relative path has no parent to create.
        if !dir.as_os_str().is_empty() {
            create_overlay_dir(dir).map_err(|e| anyhow::anyhow!("{}: {e}", dir.display()))?;
        }
    }

    // Pretty, with a trailing newline. This is a file a person will open to
    // find out what the panel decided, and to delete a line out of when they
    // want the config file to win again; one long line would make both of
    // those worse for no gain anybody can measure on a file this size.
    let mut text = serde_json::to_string_pretty(overlay)
        .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
    text.push('\n');

    // Unique per save: the pid separates processes and the counter separates
    // two saves inside one, so a fixed name cannot be planted ahead of the
    // open or collide with another saver's half-written file.
    let unique = TEMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(0);
    let temporary = path.with_file_name(format!(
        ".{}.{}.{unique:x}-{nanos:x}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("settings"),
        std::process::id()
    ));
    // Written, flushed and synced before the rename, which is what announces
    // it: without the sync a crash can leave the rename durable and the bytes
    // it points at not, which is an empty `settings.json` — fatal for
    // `--config`, and the exact failure the temp file exists to prevent.
    let written = (|| -> std::io::Result<()> {
        use std::io::Write as _;
        let mut file = create_private_temp(&temporary)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&temporary);
        return Err(anyhow::anyhow!("{}: {e}", temporary.display()));
    }
    if let Err(e) = std::fs::rename(&temporary, path) {
        // The temporary is cleaned up here rather than left behind: a rename
        // that failed is usually a read-only or full filesystem, and leaving a
        // temporary next to the config file is a puzzle for whoever finds it
        // later.
        let _ = std::fs::remove_file(&temporary);
        return Err(anyhow::anyhow!("{}: {e}", path.display()));
    }
    // The directory entry too, so the rename itself is durable.
    if let Some(parent) = path.parent() {
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

/// Counter for temporary names, so two saves in the same nanosecond differ.
static TEMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Create `dir` if it is missing, with no group or other access when this call
/// is the one that creates it. An existing directory is left as it is.
fn create_overlay_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    use std::os::unix::fs::DirBuilderExt as _;

    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(0o700);
    builder.create(dir)
}

/// Open a brand-new file only this user can read, refusing to follow a symlink
/// at the final path component.
fn create_private_temp(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path)
}

/// Read the overlay written by [`save`], if it is there.
pub fn load(path: &Path) -> anyhow::Result<Option<Overlay>> {
    match read_bounded(path) {
        Ok(text) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| anyhow::anyhow!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::anyhow!("{}: {e}", path.display())),
    }
}

/// The most overlay text this will read: a hand-written file is a few hundred
/// bytes, and a larger one is not a settings panel's output. The cap is what
/// keeps a FIFO or an enormous file in the config directory from blocking or
/// exhausting startup. `O_NOFOLLOW` refuses a final symlink for the same
/// reason.
const MAX_OVERLAY_BYTES: usize = 1 << 20;

fn read_bounded(path: &Path) -> std::io::Result<String> {
    use std::io::Read as _;

    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        // O_NONBLOCK so a FIFO at this path returns from open() instead of
        // blocking startup before the regular-file check can refuse it.
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    if metadata.len() > MAX_OVERLAY_BYTES as u64 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "larger than the overlay cap",
        ));
    }
    let mut text = String::new();
    file.take(MAX_OVERLAY_BYTES as u64 + 1)
        .read_to_string(&mut text)?;
    if text.len() > MAX_OVERLAY_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "larger than the overlay cap",
        ));
    }
    Ok(text)
}

/// The shell's copy of a bar widget back in the file's shape, for the
/// overlay to carry.
///
/// The credential env names an `ai` widget names are not in it — the shell
/// never sees them — so a saved `ai` widget reads the provider's default
/// env name again. That is the compositor's own loss in `bar_widget_ipc`,
/// predating this trip, rather than one this function adds.
fn bar_widget_config(
    widget: &viewport_ipc::event::BarWidget,
) -> Option<crate::config::BarWidgetConfig> {
    use crate::config::BarWidgetConfig as Config;
    use viewport_ipc::event::BarWidget as Event;
    Some(match widget {
        Event::Disk { path } => Config::Disk { path: path.clone() },
        Event::Weather { location } => Config::Weather {
            location: location.clone(),
        },
        Event::Volume => Config::Volume,
        Event::Mic => Config::Mic,
        Event::Mpris => Config::Mpris,
        Event::Battery => Config::Battery,
        Event::Ai { provider } => {
            let provider = match provider.as_str() {
                "claude" => crate::config::AiProvider::Claude,
                "openai" => crate::config::AiProvider::Openai,
                "openrouter" => crate::config::AiProvider::Openrouter,
                _ => return None,
            };
            Config::Ai {
                provider,
                token_env: None,
                account_id_env: None,
            }
        }
        Event::Custom { name, options } => Config::Custom {
            name: name.clone(),
            options: options.clone(),
        },
    })
}

/// The shell's copy of one bar item back in the file's shape.
fn bar_item_config(item: &viewport_ipc::event::BarItem) -> Option<crate::config::BarItemConfig> {
    use crate::config::BarItemConfig as Config;
    use viewport_ipc::event::BarItem as Event;
    Some(match item {
        Event::Module(name) => Config::Module(name.clone()),
        Event::Widget(widget) => Config::Widget(bar_widget_config(widget)?),
    })
}

/// Lay the overlay's keys over a loaded config file.
///
/// Only the keys the overlay holds: absence means "leave what the file said",
/// which is the whole reason this is a merge rather than a second
/// `apply_config`. Applying it as a `File` of its own reset every field the
/// overlay did not name — `tray`, `terminal` and the whole `binds` keymap
/// among them — so one trip through the settings panel silently discarded a
/// hand-written configuration.
pub fn apply(into: &mut crate::config::File, overlay: &Overlay) {
    if let Some(value) = overlay.dark_mode {
        into.dark_mode = Some(value);
    }
    if let Some(value) = overlay.wallpaper.as_ref() {
        into.wallpaper = Some(value.clone());
    }
    if let Some(value) = overlay.wallpaper_mode.as_ref() {
        into.wallpaper_mode = Some(value.clone());
    }
    if let Some(gaps) = overlay.gaps.as_ref() {
        if let Some(value) = gaps.inner {
            into.gaps.inner = Some(value);
        }
        if let Some(value) = gaps.outer {
            into.gaps.outer = Some(value);
        }
        if let Some(value) = gaps.smart {
            into.gaps.smart = Some(value);
        }
    }
    if let Some(border) = overlay.border.as_ref() {
        if let Some(value) = border.radius {
            into.border.radius = Some(value);
        }
        if let Some(value) = border.width {
            into.border.width = Some(value);
        }
        if let Some(value) = border.smart {
            into.border.smart = Some(value);
        }
    }
    if let Some(cursor) = overlay.cursor.as_ref() {
        if let Some(value) = cursor.hide_after_ms {
            into.cursor.hide_after_ms = Some(value);
        }
        if let Some(value) = cursor.hide_on_key_press {
            into.cursor.hide_on_key_press = Some(value);
        }
        if let Some(value) = cursor.hide_on_touch {
            into.cursor.hide_on_touch = Some(value);
        }
        if let Some(value) = cursor.theme.as_ref() {
            into.cursor.theme = Some(value.clone());
        }
        if let Some(value) = cursor.size {
            into.cursor.size = Some(value);
        }
        if let Some(value) = cursor.follow_mouse {
            into.cursor.follow_mouse = Some(value);
        }
        if let Some(value) = cursor.follow_mouse_threshold {
            into.cursor.follow_mouse_threshold = Some(value);
        }
    }
    if let Some(motion) = overlay.motion.as_ref() {
        if let Some(value) = motion.enabled {
            into.motion.enabled = Some(value);
        }
        if let Some(value) = motion.duration {
            into.motion.duration = Some(value);
        }
        if let Some(value) = motion.slow {
            into.motion.slow = Some(value);
        }
        if let Some(value) = motion.ease.as_ref() {
            into.motion.ease = Some(value.clone());
        }
    }
    if let Some(group) = overlay.group.as_ref() {
        if let Some(value) = group.auto_group {
            into.group.auto_group = Some(value);
        }
    }
    if let Some(clock) = overlay.clock.as_ref() {
        if let Some(value) = clock.locale.as_ref() {
            into.clock.locale = Some(value.clone());
        }
        if let Some(value) = clock.hour12 {
            into.clock.hour12 = Some(value);
        }
        if let Some(value) = clock.format.as_ref() {
            into.clock.format = Some(value.clone());
        }
    }
    if let Some(value) = overlay.bar.as_ref() {
        into.bar = Some(value.clone());
    }
    if let Some(value) = overlay.bar_blur {
        into.bar_blur = Some(value);
    }
    if let Some(widgets) = overlay.bar_widgets.as_ref() {
        into.bar_widgets = widgets.iter().filter_map(bar_widget_config).collect();
    }
    if let Some(items) = overlay.bar_items.as_ref() {
        into.bar_items = Some(items.iter().filter_map(bar_item_config).collect());
    }
    if let Some(value) = overlay.logo {
        into.logo = Some(value);
    }
    if let Some(value) = overlay.tutorial {
        into.tutorial = Some(value);
    }
    if let Some(value) = overlay.tiling_mode.as_ref() {
        into.tiling_mode = Some(value.clone());
    }
    if let Some(value) = overlay.layout.as_ref() {
        into.layout = Some(value.clone());
    }
    if let Some(extensions) = overlay.layout_extensions.as_ref() {
        into.layout_extensions = Some(
            extensions
                .iter()
                .map(|(name, url)| (name.clone(), url.clone()))
                .collect(),
        );
    }
    if let Some(value) = overlay.focus_crosses_outputs {
        into.focus_crosses_outputs = Some(value);
    }
    if let Some(value) = overlay.osk.as_ref() {
        into.osk = Some(value.clone());
    }
    if let Some(value) = overlay.tray {
        into.tray = Some(value);
    }
    if let Some(value) = overlay.clipboard_history {
        into.clipboard_history = Some(value);
    }
    if let Some(value) = overlay.icon_theme.as_ref() {
        into.icon_theme = Some(value.clone());
    }
    if let Some(value) = overlay.theme.as_ref() {
        into.theme = Some(value.clone());
    }
    if let Some(value) = overlay.rules.as_ref() {
        into.rules = Some(value.clone());
    }
    if let Some(value) = overlay.layer_rules.as_ref() {
        // Parsed back as the file's own rules; a value that no longer
        // parses leaves the file's standing, rather than refusing the
        // whole overlay.
        if let Ok(rules) = serde_json::from_value::<Vec<crate::layer::RuleConfig>>(value.clone()) {
            into.layer_rules = Some(rules);
        }
    }
    if let Some(value) = overlay.decorations.as_ref() {
        into.decorations = Some(value.clone());
    }
    if let Some(value) = overlay.workspaces.as_ref() {
        if let Ok(workspaces) = serde_json::from_value::<
            std::collections::HashMap<u32, crate::config::WorkspaceConfig>,
        >(value.clone())
        {
            into.workspaces = Some(workspaces);
        }
    }
    if let Some(opacity) = overlay.opacity.as_ref() {
        into.opacity.active = Some(opacity.active);
        into.opacity.inactive = Some(opacity.inactive);
        into.opacity.fullscreen = Some(opacity.fullscreen);
    }
    // Per monitor, not the whole map: the overlay holds only the monitors the
    // panel touched, and a config-file block for an untouched one has to
    // survive.
    for (name, monitor) in &overlay.outputs {
        let entry = into.outputs.entry(name.clone()).or_default();
        if let Some(value) = monitor.enabled {
            entry.enabled = Some(value);
        }
        if let Some(value) = monitor.mode.as_ref() {
            entry.mode = Some(value.clone());
        }
        if let Some(value) = monitor.scale {
            entry.scale = Some(value);
        }
        if let Some(value) = monitor.transform.as_ref() {
            entry.transform = Some(value.clone());
        }
        if let Some(value) = monitor.x {
            entry.x = Some(value);
        }
        if let Some(value) = monitor.y {
            entry.y = Some(value);
        }
        if let Some(value) = monitor.mirror.as_ref() {
            entry.mirror = Some(value.clone());
        }
        if let Some(value) = monitor.vrr {
            entry.vrr = Some(value);
        }
    }
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
                mirror: Some("HDMI-A-1".to_owned()),
                vrr: Some(viewport_ipc::event::VrrMode::Fullscreen),
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
            cursor: Some(viewport_ipc::event::Cursor {
                hide_after_ms: Some(0),
                hide_on_key_press: Some(true),
                hide_on_touch: None,
                theme: Some("Adwaita".to_owned()),
                size: Some(24),
                follow_mouse: Some(true),
                follow_mouse_threshold: Some(1.5),
            }),
            motion: Some(viewport_ipc::event::Motion {
                enabled: Some(false),
                duration: Some(120.0),
                slow: None,
                ease: Some("linear".to_owned()),
            }),
            group: Some(viewport_ipc::event::Group {
                auto_group: Some(true),
            }),
            clock: Some(viewport_ipc::event::Clock {
                locale: Some("de-DE".to_owned()),
                hour12: None,
                format: Some("%H:%M".to_owned()),
            }),
            bar: Some("docked".to_owned()),
            bar_blur: Some(false),
            bar_widgets: Some(vec![
                viewport_ipc::event::BarWidget::Disk {
                    path: Some("/home".to_owned()),
                },
                viewport_ipc::event::BarWidget::Volume,
            ]),
            bar_items: Some(vec![
                viewport_ipc::event::BarItem::Module("clock".to_owned()),
                viewport_ipc::event::BarItem::Widget(viewport_ipc::event::BarWidget::Mpris),
            ]),
            logo: Some(false),
            tutorial: Some(true),
            tiling_mode: Some("bsp".to_owned()),
            layout: Some("scrolling".to_owned()),
            layout_extensions: Some(
                [("mine".to_owned(), "file:///mine.js".to_owned())]
                    .into_iter()
                    .collect(),
            ),
            focus_crosses_outputs: Some(false),
            osk: Some("manual".to_owned()),
            tray: Some(false),
            clipboard_history: Some(10),
            icon_theme: Some("Papirus".to_owned()),
            theme: Some(serde_json::json!({"color": "red"})),
            rules: Some(serde_json::json!([{"app_id": "mpv", "capture": false}])),
            layer_rules: Some(serde_json::json!([{
                "match": {"namespace": "bar"},
                "opacity": 0.9,
                "capture": null,
                "blur": null,
                "z_index": null,
            }])),
            decorations: Some("client".to_owned()),
            workspaces: Some(serde_json::json!({
                "2": {"output": "DP-1", "layout": null, "tiling_mode": null,
                      "gaps": {"inner": 4, "outer": null, "smart": null},
                      "persistent": true, "default_name": null, "on_created_empty": null},
            })),
            opacity: Some(viewport_ipc::event::Opacity {
                active: 0.95,
                inactive: 0.8,
                fullscreen: 1.0,
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
        assert_eq!(file.cursor.hide_after_ms, Some(0));
        assert_eq!(file.cursor.hide_on_key_press, Some(true));
        assert_eq!(file.cursor.hide_on_touch, None);
        assert_eq!(file.cursor.theme.as_deref(), Some("Adwaita"));
        assert_eq!(file.cursor.size, Some(24));
        assert_eq!(file.cursor.follow_mouse, Some(true));
        assert_eq!(file.cursor.follow_mouse_threshold, Some(1.5));
        assert_eq!(file.motion.enabled, Some(false));
        assert_eq!(file.motion.duration, Some(120.0));
        assert_eq!(file.motion.slow, None);
        assert_eq!(file.motion.ease.as_deref(), Some("linear"));
        assert_eq!(file.group.auto_group, Some(true));
        assert_eq!(file.clock.locale.as_deref(), Some("de-DE"));
        assert_eq!(file.clock.hour12, None);
        assert_eq!(file.clock.format.as_deref(), Some("%H:%M"));
        assert_eq!(file.bar.as_deref(), Some("docked"));
        assert_eq!(file.bar_blur, Some(false));
        assert_eq!(file.bar_widgets.len(), 2);
        assert!(matches!(
            file.bar_widgets[0],
            crate::config::BarWidgetConfig::Disk {
                ref path
            } if path.as_deref() == Some("/home")
        ));
        assert!(matches!(
            file.bar_widgets[1],
            crate::config::BarWidgetConfig::Volume
        ));
        assert_eq!(file.bar_items.as_ref().map(Vec::len), Some(2));
        assert!(matches!(
            file.bar_items.as_deref(),
            Some([
                crate::config::BarItemConfig::Module(ref name),
                crate::config::BarItemConfig::Widget(crate::config::BarWidgetConfig::Mpris),
            ]) if name == "clock"
        ));
        assert_eq!(file.logo, Some(false));
        assert_eq!(file.tutorial, Some(true));
        assert_eq!(file.tiling_mode.as_deref(), Some("bsp"));
        assert_eq!(file.layout.as_deref(), Some("scrolling"));
        assert_eq!(
            file.layout_extensions
                .as_ref()
                .map(|m| m.get("mine").map(String::as_str)),
            Some(Some("file:///mine.js"))
        );
        assert_eq!(file.focus_crosses_outputs, Some(false));
        assert_eq!(file.osk.as_deref(), Some("manual"));
        assert_eq!(file.tray, Some(false));
        assert_eq!(file.clipboard_history, Some(10));
        assert_eq!(file.icon_theme.as_deref(), Some("Papirus"));
        assert_eq!(file.theme, Some(serde_json::json!({"color": "red"})));
        assert!(file.rules.is_some());
        assert_eq!(file.layer_rules.as_ref().map(Vec::len), Some(1));
        assert_eq!(file.decorations.as_deref(), Some("client"));
        let workspace = file.workspaces.as_ref().expect("workspaces saved");
        let workspace = workspace.get(&2).expect("workspace 2 saved");
        assert_eq!(workspace.output.as_deref(), Some("DP-1"));
        assert_eq!(workspace.gaps.inner, Some(4));
        assert_eq!(file.opacity.active, Some(0.95));
        assert_eq!(file.opacity.inactive, Some(0.8));
        assert_eq!(file.opacity.fullscreen, Some(1.0));

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
        assert_eq!(output.mirror.as_deref(), Some("HDMI-A-1"));
        assert_eq!(output.vrr, Some(viewport_ipc::event::VrrMode::Fullscreen));
    }

    /// A detached mirror and a cleared wallpaper are settings with an
    /// explicit "nothing" spelling, and the overlay has to carry that
    /// spelling rather than leaving the key out: absence means "the config
    /// file is still in charge", so a panel that removed the mirror or the
    /// picture would watch them come back on the next start.
    #[test]
    fn an_empty_mirror_or_wallpaper_clears_what_the_file_said() {
        let mut base: crate::config::File = serde_json::from_str(
            r#"{
                "wallpaper": "file:///pic/wall.png",
                "outputs": {"DP-1": {"mirror": "HDMI-A-1"}}
            }"#,
        )
        .expect("the config file");

        let mut outputs = BTreeMap::new();
        outputs.insert(
            "DP-1".to_owned(),
            OutputOverlay {
                mirror: Some(String::new()),
                ..OutputOverlay::default()
            },
        );
        let overlay = Overlay {
            wallpaper: Some(String::new()),
            outputs,
            ..Overlay::default()
        };

        apply(&mut base, &overlay);

        assert_eq!(base.wallpaper.as_deref(), Some(""));
        assert_eq!(
            base.outputs["DP-1"].mirror.as_deref(),
            Some(""),
            "a detached mirror is empty, not absent"
        );
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

    /// A saved overlay names windows and monitor arrangements, so it is no
    /// more public than a saved layout: the newly created directory is 0700
    /// and the file 0600, whatever the umask was.
    #[cfg(unix)]
    #[test]
    fn a_save_creates_private_files_and_leaves_existing_directories_alone() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir =
            std::env::temp_dir().join(format!("viewport-settings-modes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let nested = dir.join("viewport");
        let file = nested.join("settings.json");

        save(&file, &full()).expect("should write");
        assert_eq!(
            std::fs::metadata(&nested)
                .expect("the created directory")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&file)
                .expect("the overlay")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        // The unique temporary was renamed, not left beside the file.
        assert_eq!(
            std::fs::read_dir(&nested)
                .expect("the directory")
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
                .count(),
            0
        );

        // Only a directory this call created is chmodded; one that was already
        // there keeps the mode its owner chose.
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&nested).expect("the existing directory");
        std::fs::set_permissions(&nested, std::fs::Permissions::from_mode(0o755))
            .expect("setting the existing mode");
        save(&file, &Overlay::default()).expect("should write into it");
        assert_eq!(
            std::fs::metadata(&nested)
                .expect("the existing directory")
                .permissions()
                .mode()
                & 0o777,
            0o755,
            "an existing directory must not be chmodded"
        );
        assert_eq!(
            std::fs::metadata(&file)
                .expect("the overlay")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// What `save` writes is what `load` reads.
    ///
    /// The reader is a different derive from the writer, and `outputs` is a
    /// plain map rather than an `Option`, so a missing key is a parse error
    /// unless the struct has `#[serde(default)]` — which it did not, and the
    /// saved settings were then silently ignored at startup.
    #[test]
    fn a_saved_overlay_reads_back() {
        let dir =
            std::env::temp_dir().join(format!("viewport-overlay-read-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("viewport/settings.json");

        save(&file, &full()).expect("should write");
        let read = load(&file).expect("should load").expect("should be there");
        assert_eq!(read.dark_mode, Some(false));
        assert_eq!(read.gaps.as_ref().and_then(|gaps| gaps.inner), Some(12));
        assert_eq!(
            read.outputs.get("DP-1").and_then(|output| output.scale),
            Some(1.25)
        );
        // And a missing file is not an error.
        assert!(load(&dir.join("viewport/absent.json"))
            .expect("missing is fine")
            .is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The overlay is a patch, not a second config file.
    ///
    /// Applying it as one reset every field it did not name — `tray`, the
    /// terminal and the whole `binds` keymap — so a single save from the
    /// settings panel discarded a hand-written configuration.
    #[test]
    fn the_overlay_layers_over_the_file_without_resetting_it() {
        let mut base = crate::config::File {
            tray: Some(false),
            terminal: Some("kitty".to_owned()),
            binds: Some(std::collections::HashMap::new()),
            ..Default::default()
        };
        base.outputs.insert(
            "DP-2".to_owned(),
            crate::config::OutputConfig {
                scale: Some(2.0),
                ..Default::default()
            },
        );
        let mut outputs = BTreeMap::new();
        outputs.insert(
            "DP-1".to_owned(),
            OutputOverlay {
                scale: Some(1.5),
                ..Default::default()
            },
        );
        let overlay = Overlay {
            gaps: Some(viewport_ipc::event::Gaps {
                inner: Some(12),
                outer: None,
                smart: None,
            }),
            outputs,
            ..Default::default()
        };

        apply(&mut base, &overlay);

        // Keys the overlay did not name survive.
        assert_eq!(base.tray, Some(false));
        assert_eq!(base.terminal.as_deref(), Some("kitty"));
        assert!(base.binds.is_some());
        // The overlay's keys win.
        assert_eq!(base.gaps.inner, Some(12));
        // And a monitor it did not name keeps the file's block.
        assert_eq!(base.outputs.get("DP-2").and_then(|o| o.scale), Some(2.0));
        assert_eq!(base.outputs.get("DP-1").and_then(|o| o.scale), Some(1.5));
    }
}
