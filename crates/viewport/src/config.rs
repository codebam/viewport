// SPDX-License-Identifier: GPL-3.0-or-later
//
// The config file. Ports src/config.c.
//
// Every key is optional, and absence is not the same as a default: a reload
// applies only what the file actually contains, so a key left out never
// overwrites something a command-line flag set. That is why almost everything
// here is an `Option` rather than a value with a default baked in — the
// defaults live in `Config::default`, and the file is a patch over them.
//
// Unknown keys are ignored rather than refused. A config written for a later
// version has to keep working, and the alternative is a compositor that will
// not start because of a key it does not need.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// A single `outputs` entry.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct OutputConfig {
    /// Pick the highest refresh rate the mode list offers.
    pub max_refresh: Option<bool>,
    /// `WIDTHxHEIGHT` or `WIDTHxHEIGHT@RATE`.
    ///
    /// A string, and only a string. `"mode": 5` reads back as absent in C
    /// (`src/config.c:175`), so a number is not silently rounded into one.
    pub mode: Option<String>,
    pub scale: Option<f64>,
    pub transform: Option<String>,
    pub hdr: Option<bool>,
    pub x: Option<i32>,
    pub y: Option<i32>,
}

/// The keyboard block.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct KeyboardConfig {
    pub layout: Option<String>,
    pub variant: Option<String>,
    pub options: Option<String>,
    pub repeat_rate: Option<i32>,
    pub repeat_delay: Option<i32>,
}

/// The cursor block.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct CursorConfig {
    pub theme: Option<String>,
    pub size: Option<u32>,
}

/// The idle block.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct IdleConfig {
    pub lock_after: Option<i64>,
    pub blank_after: Option<i64>,
    pub lock_command: Option<String>,
}

/// What the file says. Everything optional; see the module comment.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct File {
    pub url: Option<String>,
    pub fallback: Option<String>,
    pub timeout_ms: Option<i64>,
    pub terminal: Option<String>,
    pub menu: Option<String>,
    pub startup: Option<String>,
    pub layout: Option<String>,
    pub logo: Option<bool>,
    pub tutorial: Option<bool>,
    pub bar: Option<String>,
    pub dark_mode: Option<bool>,
    pub adaptive_sync: Option<bool>,
    pub vt_switching: Option<bool>,

    /// `"client"` gives the frame back to the client; anything else, including
    /// absence, keeps it here (`src/config.c:315`).
    pub decorations: Option<String>,

    /// Handed to the shell as parsed JSON rather than re-serialised text, so
    /// it does not parse twice inside a message it already parsed.
    pub rules: Option<serde_json::Value>,
    pub theme: Option<serde_json::Value>,

    pub outputs: std::collections::HashMap<String, OutputConfig>,
    pub keyboard: KeyboardConfig,
    pub cursor: CursorConfig,
    pub idle: IdleConfig,

    /// The whole keymap. Presence means "these and no built-ins", which is why
    /// an empty `"binds": {}` is meaningful — it asks for none at all.
    pub binds: Option<std::collections::HashMap<String, Option<String>>>,

    /// Changes to the built-in keymap rather than a replacement for it.
    ///
    /// Rebinding one chord through `binds` costs all the others, because
    /// presence of that key means "this is the whole keymap" — so overriding
    /// one line meant copying the entire default list and keeping it in step
    /// with every later release, and the copy goes stale silently.
    ///
    /// A null claims the chord and does nothing with it, which is how a
    /// default is removed rather than replaced. Without it there is no way to
    /// say "this chord must reach the application", because leaving it out is
    /// exactly what asks for the built-in back.
    pub binds_override: Option<std::collections::HashMap<String, Option<String>>>,
}

/// `$XDG_CONFIG_HOME/viewport/config.json`, or `~/.config` (`src/config.c:76`).
pub fn default_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir).join("viewport/config.json"));
        }
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".config/viewport/config.json"))
}

/// Read and parse a config file.
///
/// `Ok(None)` means there was no file. A missing default is ordinary and a
/// missing `--config` is not, but that judgement belongs to the caller, which
/// is the only place that knows which one this is.
pub fn load(path: &Path) -> anyhow::Result<Option<File>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(anyhow::anyhow!("{}: {e}", path.display())),
    };
    // The line and column come from serde_json, which is what makes a
    // misplaced comma findable rather than "config is invalid".
    let file: File = serde_json::from_str(&text)
        .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
    Ok(Some(file))
}

/// The binding specifications a file asks for, in the form `parse` takes.
///
/// `binds` replaces the defaults and `binds_override` layers over them, but
/// both produce the same `chord=action` strings — the difference is only
/// whether the defaults were added first, which is the caller's business.
pub fn bind_specs(binds: &std::collections::HashMap<String, Option<String>>) -> Vec<String> {
    let mut specs: Vec<String> = binds
        .iter()
        .map(|(chord, action)| {
            // A null unbinds: the chord is claimed and does nothing, so the
            // built-in does not come back.
            let action = action.as_deref().unwrap_or("none");
            format!("{chord}={action}")
        })
        .collect();
    // A HashMap has no order and bindings are matched first-wins, so without
    // this the same file could produce different behaviour between runs.
    specs.sort();
    specs
}

/// Parse a mode string: `WIDTHxHEIGHT` or `WIDTHxHEIGHT@RATE`.
///
/// The refresh rate is in Hz with optional decimals and comes back in mHz,
/// which is what DRM and the IPC protocol both use. Without a rate the answer
/// is a resolution to match on, which is why it is optional rather than zero
/// — zero is a real refresh rate to a mode search.
pub fn parse_mode(text: &str) -> Option<(i32, i32, Option<i32>)> {
    let (size, rate) = match text.split_once('@') {
        Some((size, rate)) => (size, Some(rate)),
        None => (text, None),
    };
    let (width, height) = size.trim().split_once('x')?;
    let width: i32 = width.trim().parse().ok()?;
    let height: i32 = height.trim().parse().ok()?;
    if width <= 0 || height <= 0 {
        return None;
    }
    let rate = match rate {
        Some(rate) => {
            let hz: f64 = rate.trim().parse().ok()?;
            if hz <= 0.0 {
                return None;
            }
            Some((hz * 1000.0).round() as i32)
        }
        None => None,
    };
    Some((width, height, rate))
}

/// The mode a configuration block asks for, if the connector offers it.
///
/// `"mode": "2560x1440@240"` names one exactly; without a rate it is the
/// fastest mode of that size, because a resolution alone almost always means
/// "that size, as fast as it goes". `"max_refresh": true` is the same question
/// without a size: the fastest mode the display has.
///
/// A rate is matched to the nearest whole hertz. The kernel reports 239765
/// millihertz where a person writes 240, and a configuration that has to be
/// exact to three decimal places is a configuration nobody can write.
pub fn pick_mode(
    modes: &[smithay::reexports::drm::control::Mode],
    config: &OutputConfig,
) -> Option<smithay::reexports::drm::control::Mode> {
    let fastest = |candidates: &mut dyn Iterator<Item = &smithay::reexports::drm::control::Mode>| {
        candidates
            .max_by_key(|mode| mode.vrefresh())
            .copied()
    };

    if let Some(spec) = config.mode.as_deref() {
        let (size, rate) = match spec.split_once('@') {
            Some((size, rate)) => (size, rate.trim().parse::<f64>().ok()),
            None => (spec, None),
        };
        let (width, height) = size.trim().split_once('x')?;
        let width: u16 = width.trim().parse().ok()?;
        let height: u16 = height.trim().parse().ok()?;
        let matching = modes.iter().filter(|mode| mode.size() == (width, height));

        return match rate {
            Some(rate) => {
                let wanted = rate.round() as u32;
                matching
                    .clone()
                    .find(|mode| mode.vrefresh() == wanted)
                    .copied()
                    .or_else(|| {
                        // Nothing at that rate. The size is the part the user
                        // can see, so it wins, and the miss is said out loud.
                        let closest = fastest(&mut matching.clone());
                        if closest.is_some() {
                            tracing::warn!(
                                "no {width}x{height}@{wanted} mode; using the fastest {width}x{height}"
                            );
                        }
                        closest
                    })
            }
            None => fastest(&mut matching.clone()),
        };
    }

    if config.max_refresh == Some(true) {
        // The fastest mode at the largest size, rather than the fastest mode
        // outright: a display will happily offer 360Hz at a resolution nobody
        // wants to use.
        let widest = modes.iter().map(|mode| mode.size()).max()?;
        return fastest(&mut modes.iter().filter(|mode| mode.size() == widest));
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mode_string_parses_the_way_the_c_build_wrote_them() {
        // `src/config.c:145` documents both forms.
        assert_eq!(parse_mode("2560x1440"), Some((2560, 1440, None)));
        assert_eq!(
            parse_mode("2560x1440@239.760"),
            Some((2560, 1440, Some(239_760)))
        );
        // Hz to mHz, which is what DRM and the protocol both use.
        assert_eq!(parse_mode("1920x1080@60"), Some((1920, 1080, Some(60_000))));
    }

    #[test]
    fn a_mode_string_that_is_not_one_is_refused() {
        // Rather than becoming a zero, which is a real refresh rate to a mode
        // search and would silently match nothing.
        for bad in ["", "2560", "2560x", "x1440", "2560x1440@", "0x1440", "2560x1440@0"] {
            assert_eq!(parse_mode(bad), None, "{bad:?} should not parse");
        }
    }

    #[test]
    fn an_empty_object_is_a_valid_config() {
        // Every key is optional, so this is the file that changes nothing.
        let file: File = serde_json::from_str("{}").expect("should parse");
        assert_eq!(file, File::default());
        assert!(file.logo.is_none(), "absent is not false");
    }

    #[test]
    fn an_unknown_key_is_ignored() {
        // A config written for a later version has to keep working. Refusing
        // it means a compositor that will not start over a key it does not
        // need.
        let file: File =
            serde_json::from_str(r#"{"terminal":"foot","invented_later":true}"#)
                .expect("should parse");
        assert_eq!(file.terminal.as_deref(), Some("foot"));
    }

    #[test]
    fn absence_and_false_stay_different() {
        // The whole reason these are Options. A reload applies only what the
        // file contains, so "logo" left out must not turn the logo off.
        let absent: File = serde_json::from_str("{}").unwrap();
        let present: File = serde_json::from_str(r#"{"logo":false}"#).unwrap();
        assert_eq!(absent.logo, None);
        assert_eq!(present.logo, Some(false));
    }

    #[test]
    fn a_null_bind_unbinds_rather_than_being_dropped() {
        // Without this there is no way to say "this chord must reach the
        // application": leaving it out is what asks for the built-in back.
        let file: File =
            serde_json::from_str(r#"{"binds_override":{"Mod4+d":null,"Mod4+Return":"exec foot"}}"#)
                .expect("should parse");
        let specs = bind_specs(file.binds_override.as_ref().unwrap());
        assert!(specs.contains(&"Mod4+d=none".to_owned()));
        assert!(specs.contains(&"Mod4+Return=exec foot".to_owned()));
    }

    #[test]
    fn an_empty_binds_block_is_not_the_same_as_no_binds_block() {
        // Presence means "this is the whole keymap", so an empty one asks for
        // no defaults at all.
        let empty: File = serde_json::from_str(r#"{"binds":{}}"#).unwrap();
        let absent: File = serde_json::from_str("{}").unwrap();
        assert_eq!(empty.binds.as_ref().map(|b| b.len()), Some(0));
        assert!(absent.binds.is_none());
    }

    #[test]
    fn bind_order_does_not_depend_on_the_hash() {
        // Bindings are matched first-wins and a HashMap has no order, so
        // without sorting the same file could behave differently between runs.
        let file: File = serde_json::from_str(
            r#"{"binds":{"Mod4+b":"exec b","Mod4+a":"exec a","Mod4+c":"exec c"}}"#,
        )
        .unwrap();
        let specs = bind_specs(file.binds.as_ref().unwrap());
        let mut sorted = specs.clone();
        sorted.sort();
        assert_eq!(specs, sorted);
    }

    #[test]
    fn the_nested_blocks_parse() {
        let file: File = serde_json::from_str(
            r#"{
                "keyboard": {"layout":"us","repeat_rate":25},
                "cursor": {"theme":"Adwaita","size":32},
                "idle": {"lock_after":300,"lock_command":"swaylock"},
                "outputs": {"DP-1":{"mode":"2560x1440@120","x":0,"hdr":true}}
            }"#,
        )
        .expect("should parse");
        assert_eq!(file.keyboard.layout.as_deref(), Some("us"));
        assert_eq!(file.keyboard.repeat_rate, Some(25));
        assert_eq!(file.cursor.size, Some(32));
        assert_eq!(file.idle.lock_after, Some(300));
        let dp1 = file.outputs.get("DP-1").expect("DP-1");
        assert_eq!(dp1.mode.as_deref(), Some("2560x1440@120"));
        assert_eq!(dp1.hdr, Some(true));
        // Absent within a block is still absent, not zero.
        assert_eq!(dp1.y, None);
    }

    #[test]
    fn rules_and_theme_stay_json() {
        // They go to the shell as parsed JSON rather than a string, so it does
        // not parse twice inside a message it already parsed.
        let file: File = serde_json::from_str(
            r##"{"rules":[{"app_id":"foot","floating":true}],"theme":{"accent":"#ff0000"}}"##,
        )
        .expect("should parse");
        assert!(file.rules.as_ref().unwrap().is_array());
        assert!(file.theme.as_ref().unwrap().is_object());
    }

    #[test]
    fn a_missing_file_is_not_an_error() {
        let missing = std::path::Path::new("/nonexistent/viewport/config.json");
        assert!(matches!(load(missing), Ok(None)));
    }

    #[test]
    fn a_malformed_file_names_where() {
        let dir = std::env::temp_dir().join("viewport-config-test");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("bad.json");
        std::fs::write(&path, "{ \"terminal\": }").expect("write");
        let error = load(&path).expect_err("should fail").to_string();
        // The path and the position, so a misplaced comma is findable rather
        // than "config is invalid".
        assert!(error.contains("bad.json"), "{error}");
        assert!(error.contains("line"), "{error}");
        let _ = std::fs::remove_file(&path);
    }
}
