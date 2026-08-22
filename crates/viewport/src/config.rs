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
    /// Milliseconds of no pointer input before the cursor is taken off the
    /// screen. Zero or absent is off, as for every other deadline here.
    ///
    /// Milliseconds rather than the seconds the idle block uses, because this
    /// deadline is a second or two and the useful settings are not whole
    /// numbers of seconds — sway spells the same setting the same way.
    pub hide_after_ms: Option<i64>,
}

/// The `magnify` block: the screen magnifier's step and its ceiling.
///
/// Only the two numbers, because everything else about the magnifier is a
/// chord rather than a setting — it is off until somebody presses zoom-in,
/// and where it looks is wherever the pointer is. Absent is a step of 0.5 and
/// a maximum of 8.0; see [`crate::magnify`] for why those, and for what
/// happens to a file that asks for something impossible.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct MagnifyConfig {
    pub step: Option<f64>,
    pub max: Option<f64>,
}

/// The idle block.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct IdleConfig {
    pub lock_after: Option<i64>,
    pub blank_after: Option<i64>,
    pub lock_command: Option<String>,
}

/// The `notifications` block.
///
/// What a notification that names no sound of its own sounds like. The keys
/// are the specification's two sound hints, spelled the way every other key
/// here is, and a sender that sets either hint overrides them for its own
/// notification — see `crate::notification::sound`.
///
/// Absent is silence, which is what this compositor did before it could play
/// anything at all. There is deliberately no built-in default sound: the
/// freedesktop sound theme has no notification event, so a name picked here
/// would be one distribution's choice imposed on everyone else's.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct NotificationsConfig {
    /// A path to a sound file, played as given.
    pub sound_file: Option<String>,
    /// A name from the sound naming specification, resolved against the
    /// installed sound theme — `"message-new-instant"`, `"bell"`.
    ///
    /// Ignored when `sound_file` is set, for the reason `Sound::from_config`
    /// gives: a path always resolves and a name may resolve to nothing.
    pub sound_name: Option<String>,

    /// How many notifications the centre keeps after their popups have gone.
    ///
    /// Absent is 50. Zero turns the record off: a popup is drawn and then it
    /// is nothing, which is what this compositor did before it kept any, and
    /// what a session that would rather not have a list of everything that
    /// notified it asks for.
    ///
    /// The setting applies on reload, so lowering it drops the oldest entries
    /// there and then rather than at the next restart.
    pub history: Option<usize>,
}

/// The `gaps` block.
///
/// The shell spaces windows with a pair of CSS custom properties: the inner
/// gap between adjacent windows, and the outer gap at the edge of the output.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct GapsConfig {
    /// Pixels between adjacent windows, as sway's `gaps.inner`. Absent keeps
    /// the shell's own default, which is 8.
    pub inner: Option<i32>,
    /// Extra pixels around the edge of the output, added *outside* the inner
    /// gap — sway's `gaps.outer`. So the space where the desktop meets the
    /// screen edge is `inner + outer`, while between two windows it is still
    /// just `inner`. Absent means 0 (the edge is only the inner gap).
    pub outer: Option<i32>,
    /// When true and a workspace holds exactly one window that fills the tiling
    /// area, only the outer gap is applied and the inner one is dropped — so a
    /// lone window does not get a large border on its empty workspace. Sway's
    /// `smart_gaps`. In the scrolling layout a lone column narrower than the
    /// output does not reach the screen edge to begin with and keeps both gaps.
    /// Absent means off (a single window keeps both gaps).
    pub smart: Option<bool>,
}

/// The `border` block.
///
/// The frame the shell draws around a window. Read on both sides: the shell
/// draws the corner, and the compositor crops the client to it — a rounded
/// border with a square client over it is a border that is not there.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct BorderConfig {
    /// The corner radius in logical pixels, on the outside of the border.
    /// Absent keeps the shell's own default, which is `--radius` (6). Zero is
    /// square corners, and turns the cropping off along with them.
    pub radius: Option<i32>,
    /// How thick the border is, in logical pixels. Absent keeps the shell's
    /// own default of 2. Zero is no border at all — the windows then meet the
    /// gap between them directly, which is a desktop that separates windows by
    /// space rather than by a line.
    ///
    /// The compositor reads it for the same reason it reads the radius: the
    /// client's corner is the outer one less the border, so a thicker border
    /// is a tighter curve on the surface inside it.
    pub width: Option<i32>,
    /// Square the corners of a workspace's lone window, the way `gaps.smart`
    /// drops its inner gap — sway's `smart_borders` for the same window.
    ///
    /// Absent follows `gaps.smart` rather than meaning off, because the two
    /// are one decision: smart gaps push that window against the edge of the
    /// screen, and a rounded corner there is a notch of wallpaper in the
    /// corner of the monitor. Set it explicitly to have one without the other.
    pub smart: Option<bool>,
}

/// The `clock` block.
///
/// The bar's clock and the calendar under it. The shell had neither of these
/// as settings: it passed the literal `"en-US"` to `toLocaleDateString` and
/// assembled the time out of `getHours()`, so every desk in the world read an
/// American date and a 24-hour clock whether or not that is how it writes one.
///
/// Nothing here is validated beyond its type. A language tag the engine has no
/// data for is the engine's business — it falls back to the system locale and
/// says so in the shell's console — and a format string is a template the page
/// expands, not a thing the compositor can be wrong about.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct ClockConfig {
    /// A BCP 47 language tag — `"de-DE"`, `"ja-JP"`, `"en-GB"` — used for the
    /// month and weekday names, the order the date is written in, and the day
    /// the calendar's week starts on.
    ///
    /// Absent is the locale the engine itself runs under, which is what
    /// `LANG` and friends already say. That is the answer somebody who has set
    /// up their system expects, and it is the one this had no way of giving.
    pub locale: Option<String>,
    /// True for a twelve-hour clock with an AM/PM after it, false for a
    /// twenty-four-hour one. Absent is whichever the locale writes, so a desk
    /// that only wants its own convention sets `locale` and nothing else.
    pub hour12: Option<bool>,
    /// A strftime-style template for the whole module — `"%a %d %b  %H:%M"` —
    /// when the shipped shape is not the wanted one.
    ///
    /// A string rather than more booleans because the thing people change is
    /// the *arrangement*: the seconds, the week number, the date after the
    /// time rather than before it, no leading glyph. Every flag that could be
    /// added here would be a worse spelling of one line of `date(1)`, and the
    /// list would never be finished. The locale-dependent conversions in it —
    /// `%A`, `%B`, `%p`, `%x` — still go through `locale`, so a template is a
    /// layout and not a second place to write English.
    ///
    /// Absent draws the shipped shape: the clock glyph, then the date and time
    /// as the locale writes them.
    pub format: Option<String>,
}

/// The corner the shell rounds a window to when the config says nothing:
/// `--radius` in `data/shell/shell.css`, which is what `.window` inherits.
///
/// Named here because the compositor needs the same number the page used —
/// it crops the client to that corner — and a shell that has not sent a
/// config yet has already drawn one. Keep it in step with the stylesheet.
pub const DEFAULT_BORDER_RADIUS: i32 = 6;

/// The thickness of `.window`'s border in the same stylesheet, when the config
/// says nothing.
///
/// The radius in the config is the one on the *outside* of the border, which
/// is where a person looking at the screen sees the corner. The client sits
/// inside the border, so its own corner is this much tighter — the rule CSS
/// applies to a padding box, done on this side because the compositor is what
/// cuts the client.
pub const DEFAULT_BORDER_WIDTH: i32 = 2;

/// One entry in a `bar_items` override: either a module the bar already knows
/// how to draw, or an extra widget.
///
/// Untagged, so a config file writes a bare string for a module (`"net"`) or
/// an object for a widget (`{"type":"disk",...}`). The shell builds the whole
/// right side of the bar from the list, in order — which is how a widget ends
/// up anywhere the built-ins can sit, not just after them.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum BarItemConfig {
    /// A module the bar draws by default: `mode`, `clock`, `cpu`, `memory`,
    /// `load`, `disk` or `net`.
    Module(String),
    /// An extra widget, taking the same options as a `bar_widgets` entry.
    Widget(BarWidgetConfig),
}

/// One extra bar widget, beyond the modules the bar ships with.
///
/// The default bar is untouched: nothing here changes what it draws, it only
/// lets a config file add to it, one widget per entry. Tagged on `type`, so a
/// config file names the kind and then the options that kind takes.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum BarWidgetConfig {
    /// Free space on a particular mount. `path` defaults to `/` when absent.
    ///
    /// The free and total bytes are sampled by the compositor, like the bar's
    /// own disk module — the page cannot read statvfs any more than it can
    /// read /proc.
    #[serde(rename = "disk")]
    Disk {
        /// The mount point to report, e.g. `"/home"` or `"/mnt/data"`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
    /// Current conditions for a location. Only this widget is fetched by the
    /// shell, which talks to a public weather service; the compositor has
    /// nothing to sample for it.
    #[serde(rename = "weather")]
    Weather {
        /// A place the weather service can find, e.g. `"New York"`. Absent is
        /// no widget worth drawing, so the shell shows nothing for it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        location: Option<String>,
    },
    /// The default audio sink's volume and mute state, sampled by the
    /// compositor from the session's PipeWire over `wpctl`.
    #[serde(rename = "volume")]
    Volume,
    /// The default audio source's (microphone) volume and mute state, sampled
    /// by the compositor over `wpctl`, like `volume` but reading
    /// `@DEFAULT_AUDIO_SOURCE@`.
    #[serde(rename = "mic")]
    Mic,
    /// What is playing, over MPRIS, with the buttons to drive it.
    ///
    /// Sampled by the compositor because the page has no bus — and only when
    /// this widget is on the bar: a desktop with no media widget does not
    /// follow every player on the session.
    #[serde(rename = "mpris")]
    Mpris,
    /// Charge and charging state, from UPower, plus a click that opens the
    /// power-profile picker.
    ///
    /// Sampled by the compositor because the page has no bus — and only when
    /// this widget is on the bar. Lid policy can still talk to UPower without
    /// one.
    #[serde(rename = "battery")]
    Battery,
}

/// What `background_terminal` was set to.
///
/// Two shapes because there are two things people mean by it: `true` is "the
/// terminal I already have configured", and a string is a command line for
/// when the point is the program inside it rather than the terminal — which is
/// the usual case, because the wallpaper is never typed into.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum BackgroundTerminal {
    Enabled(bool),
    Command(String),
}

/// What the file says. Everything optional; see the module comment.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct File {
    pub url: Option<String>,
    /// Whether `url` spans every monitor rather than taking the first and
    /// leaving the rest to the shipped desktop. See `shell_client::plan_shells`.
    pub url_span: Option<bool>,
    /// Draw a terminal as the wallpaper: `true` for the configured
    /// `terminal`, or a command line of its own.
    ///
    /// It is never given keyboard or pointer input — see
    /// `crate::background`, which explains why that is deliberate and not a
    /// missing feature.
    pub background_terminal: Option<BackgroundTerminal>,
    /// An image to draw as the wallpaper: a path, or a URL of its own.
    ///
    /// The shell paints the desktop background, so this is carried across to
    /// it rather than drawn here — see `crate::state::apply_config`. An empty
    /// string is "no wallpaper", which is how a file takes one away again
    /// without the key having to be a nullable string that also means absent.
    pub wallpaper: Option<String>,
    /// How that image is fitted to the screen: `fill`, `fit`, `stretch`,
    /// `center` or `tile`. Absent is `fill`, which is what a wallpaper is
    /// almost always meant to do.
    ///
    /// The names are stylix's `imageScalingMode`, so a themed NixOS session
    /// can hand its setting over unchanged. See [`parse_wallpaper_mode`].
    pub wallpaper_mode: Option<String>,
    /// Which engine draws the shell: `wpe`, `webkitgtk`, `chromium`, `cef`,
    /// `servo` or `servoshell`.
    /// See `crate::shell_backend`.
    pub shell_backend: Option<String>,
    pub fallback: Option<String>,
    pub timeout_ms: Option<i64>,
    pub terminal: Option<String>,
    pub menu: Option<String>,
    pub startup: Option<String>,
    pub layout: Option<String>,
    pub logo: Option<bool>,
    pub tutorial: Option<bool>,
    pub bar: Option<String>,

    /// Whether the compositor claims the system tray.
    ///
    /// Absent is on. False and it claims neither `StatusNotifierWatcher` nor a
    /// host name, which is what a session that would rather run waybar's tray
    /// — or one that wants no tray at all — asks for. The setting is applied
    /// on reload as well as at startup: turning it off releases the names and
    /// empties the bar, and applications see the tray go away exactly as they
    /// would if this program had exited.
    pub tray: Option<bool>,

    /// How many things the clipboard keeps.
    ///
    /// Absent is 25. Zero turns the history off: nothing is read, nothing is
    /// kept, and the picker has nothing to show — which is what a session that
    /// would rather run cliphist, or one that does not want a copy of every
    /// password that passes through the clipboard, asks for.
    ///
    /// Only the clipboard and only text. Recording the primary selection would
    /// mean an entry for every word dragged over with a mouse, and an image is
    /// megabytes with nowhere to draw it.
    pub clipboard_history: Option<usize>,

    /// Which icon theme a tray item's icon name is resolved against.
    ///
    /// Absent is `hicolor`, which is searched in any case — this is the theme
    /// searched *before* it. There is no way to ask a Wayland session what its
    /// icon theme is; GTK keeps it in dconf and Qt in an ini file, and neither
    /// is a thing a compositor should be reading.
    pub icon_theme: Option<String>,

    /// Whether Mod4+h and Mod4+l — and the other directional focus keys —
    /// may step onto the next monitor once there is nothing left to reach on
    /// this one.
    ///
    /// True is the long-standing behaviour and stays the default. False keeps
    /// focus on the monitor it is on, which is what someone wants when the
    /// rightmost window on the left screen is one keypress away from losing
    /// their place entirely.
    pub focus_crosses_outputs: Option<bool>,

    /// How the tiling tree arranges itself: `"manual"`, `"master-stack"`,
    /// `"spiral"`, `"bsp"` or `"grid"`. Absent is `"manual"`.
    ///
    /// Carried across to the shell rather than acted on: the compositor has no
    /// layout, so what an arrangement *is* belongs there.
    pub tiling_mode: Option<String>,

    pub dark_mode: Option<bool>,

    /// How many bits per colour channel a scanout buffer carries: `"8"`,
    /// `"10"`, or `"auto"`. Absent is `"auto"`.
    ///
    /// See [`parse_pixel_format`] for what each one does.
    pub pixel_format: Option<String>,

    /// Which graphics card renders and, on a single-GPU machine, scans out.
    ///
    /// Matched as a substring of the device path, so `"card1"`, `"renderD129"`
    /// and a whole `/dev/dri/by-path/...` all work. Absent lets the compositor
    /// rank the cards itself, which prefers one that a Vulkan device actually
    /// exposes and then whatever the seat calls primary.
    ///
    /// The setting exists because on a hybrid laptop the ranking cannot decide
    /// for you: the integrated GPU is the battery answer and the discrete one
    /// is the frames answer, and that is a preference, not something readable
    /// off the hardware. Every card is opened either way — this names the one
    /// clients allocate against and the one the shell draws on.
    pub gpu: Option<String>,

    /// What clients are told to allocate against when there is more than one
    /// card: `"native"` or `"portable"`. Absent is `"native"`.
    ///
    /// See [`crate::multigpu::CrossGpu`] for what each one costs. Nothing on a
    /// machine with one card.
    pub cross_gpu: Option<String>,

    pub adaptive_sync: Option<bool>,
    pub vt_switching: Option<bool>,

    /// Whether the on-screen keyboard is allowed to raise itself, and if not,
    /// whether `Mod4+Shift+k` still can: `"auto"`, `"manual"` or `"off"`.
    /// Absent is `"auto"`. See [`OskMode`] for what each one means and why a
    /// boolean was not enough, and `ViewportState::sync_osk_wanted` in
    /// `input.rs` for how `"auto"` decides.
    pub osk: Option<String>,

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
    pub magnify: MagnifyConfig,
    pub idle: IdleConfig,

    /// What to do when the laptop lid closes: `"ignore"`, `"lock"`, `"blank"`
    /// or `"suspend"`. Absent is lock when `idle.lock_command` is set,
    /// otherwise blank. A desktop has no lid, so the setting never fires.
    pub lid: Option<String>,

    pub gaps: GapsConfig,
    pub border: BorderConfig,
    pub notifications: NotificationsConfig,

    /// The bar clock's locale and format; see [`ClockConfig`]. Absent leaves
    /// the shell to the locale the engine runs under.
    pub clock: ClockConfig,

    /// What X11 clients are told about this desk's pixel density. See
    /// [`XwaylandConfig`], and `docs/protocols.md` for the decision behind
    /// the default being "nothing".
    pub xwayland: XwaylandConfig,

    /// Extra widgets to add to the bar, beyond the modules it draws by default.
    ///
    /// Empty or absent is the default bar, untouched. See `BarWidgetConfig`.
    pub bar_widgets: Vec<BarWidgetConfig>,

    /// Override the entire right side of the bar with an explicit, ordered
    /// list of modules and widgets.
    ///
    /// When present (even empty), it completely replaces the default module
    /// set and any `bar_widgets` — the shell draws exactly what is listed, in
    /// order. A bare string names a built-in module (`"net"`, `"cpu"`,
    /// `"clock"`, ...); an object is a widget, as in `bar_widgets`. Absent is
    /// the default bar.
    pub bar_items: Option<Vec<BarItemConfig>>,

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
    let file: File =
        serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
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
    let fastest = |candidates: &mut dyn Iterator<
        Item = &smithay::reexports::drm::control::Mode,
    >| { candidates.max_by_key(|mode| mode.vrefresh()).copied() };

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

/// Turn what someone typed after `--url` into something WebKit will load.
///
/// Two things go wrong otherwise, and both look identical from the outside —
/// an empty desktop, with the fallback page eventually taking over:
///
///   a bare path      `--url /home/me/shell/index.html` is the obvious thing
///                    to type and is not a URL. WebKit is handed it verbatim
///                    and has no scheme to resolve it with.
///
///   a space          `file:///home/me/Telegram Desktop/index.html` is not a
///                    valid URI. A space has to be `%20`; unencoded, the URL
///                    is rejected or truncated at the space, and the shell
///                    that does load is not the one that was asked for.
///
/// A path is made absolute, checked, and encoded. A URL already carrying a
/// scheme is left alone apart from its spaces, because everything else in it
/// may be deliberately encoded already and re-encoding would turn `%20` into
/// `%2520`.
pub fn shell_url(value: &str) -> anyhow::Result<String> {
    resolve_url(value, "--url")
}

/// What `wallpaper`, `--wallpaper` and `config.wallpaper` were given: a
/// picture, or a colour to paint the desktop instead.
///
/// A picture is named the way `--url` is — a path, usually, because that is
/// what a file manager and a theme generator both hand you — and reaches the
/// shell as a URL in a CSS `background-image`. So the resolution is identical,
/// down to the encoding: a wallpaper under `~/Pictures/Wall Papers` is not a
/// URI until its space is `%20`, and unencoded the page silently draws its own
/// gradient instead.
///
/// A CSS value is passed through untouched. Not every wallpaper is a
/// photograph: `#1a1b26` is what somebody with a colour scheme wants, and a
/// `linear-gradient(...)` is what somebody replacing the shipped one wants —
/// neither is a file, and turning them into one is the difference between the
/// setting doing what it says and refusing what was asked. See
/// [`looks_like_css`] for how the two are told apart.
///
/// The name in the error is the thing the caller was given, so a bad path in
/// the config file says `wallpaper` and one on the command line says
/// `--wallpaper`.
pub fn wallpaper_value(value: &str, named: &str) -> anyhow::Result<String> {
    let trimmed = value.trim();
    if looks_like_css(trimmed) {
        return Ok(trimmed.to_owned());
    }
    resolve_url(trimmed, named).map_err(|e| {
        // A bare word is almost always a colour somebody wrote by name, and
        // "black does not exist" is a message about the wrong thing. Named
        // colours are not accepted, because a relative path is a real way to
        // name a picture and there is no telling the two apart.
        if !trimmed.contains('/') && !trimmed.contains('.') {
            return anyhow::anyhow!(
                "{e}. A colour has to be written as #rrggbb or rgb(...), not by name"
            );
        }
        e
    })
}

/// Whether a wallpaper is a CSS value rather than a picture to go and find.
///
/// Three shapes, and deliberately no more: `#rrggbb`, anything of the form
/// `name(...)` — which covers `rgb()`, `hsl()`, every `gradient()` and `url()`
/// itself — and the keyword `transparent`. Everything else is a path, because
/// a path is what the setting is mostly given and a guess that goes the wrong
/// way is a picture that never appears.
///
/// A file whose name ends in `)` stays a path: the part before the `(` has to
/// look like a CSS identifier, and `/home/me/holiday (1)` does not.
fn looks_like_css(text: &str) -> bool {
    if text.starts_with('#') || text.eq_ignore_ascii_case("transparent") {
        return true;
    }
    let Some(rest) = text.strip_suffix(')') else {
        return false;
    };
    let Some((name, _)) = rest.split_once('(') else {
        return false;
    };
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// A path or a URL, as something a web engine will load.
///
/// `named` is what the value was called where it came from, and is what an
/// error names — the whole point of the message is that somebody can find the
/// thing they typed.
fn resolve_url(value: &str, named: &str) -> anyhow::Result<String> {
    let trimmed = value.trim();
    anyhow::ensure!(!trimmed.is_empty(), "{named} was given nothing to load");

    if let Some((scheme, rest)) = trimmed.split_once("://") {
        let encoded = format!("{scheme}://{}", rest.replace(' ', "%20"));
        // Only a local file can be checked, and only a local file is worth
        // checking: a http:// shell that is not up yet is a waiting game, not
        // a mistake.
        if scheme == "file" {
            let path = std::path::PathBuf::from(percent_decode(rest));
            anyhow::ensure!(path.exists(), "{named}: {} does not exist", path.display());
        }
        return Ok(encoded);
    }

    // No scheme, so it is a path — which is what someone types when they have
    // just unpacked a package and is pointing at the file they can see.
    //
    // A leading `~` is expanded here because a config file is not a shell:
    // `"wallpaper": "~/Pictures/wall.png"` is the obvious thing to write, and
    // nothing else in the process would ever turn it into a directory. On the
    // command line the shell has usually done it already, and doing it twice
    // costs nothing.
    let expanded = match trimmed.strip_prefix("~/") {
        Some(rest) => match std::env::var_os("HOME") {
            Some(home) => std::path::PathBuf::from(home).join(rest),
            None => std::path::PathBuf::from(trimmed),
        },
        None => std::path::PathBuf::from(trimmed),
    };
    let path = expanded.as_path();
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    anyhow::ensure!(
        absolute.exists(),
        "{named}: {} does not exist",
        absolute.display()
    );
    Ok(format!(
        "file://{}",
        encode_path(&absolute.to_string_lossy())
    ))
}

/// How a wallpaper image is fitted to the screen.
///
/// The five names are stylix's `imageScalingMode`, spelled exactly as it
/// spells them, because a NixOS desktop themed by stylix hands this setting
/// straight across and a second vocabulary for the same five behaviours is a
/// translation table somebody has to write. sway's `output bg` uses four of
/// the five under other names; those are accepted as aliases.
pub const WALLPAPER_MODES: [&str; 5] = ["fill", "fit", "stretch", "center", "tile"];

/// What `wallpaper_mode`, `--wallpaper-mode` and `config.wallpaper` accept.
///
/// The stylix names, plus sway's for the same thing: `cover` is `fill`,
/// `contain` is `fit`, `stretch` is already shared. Unknown is an error rather
/// than a silent `fill`, because the mode is the difference between a photo
/// and a stretched photo and a typo would look like the setting doing nothing.
pub fn parse_wallpaper_mode(value: &str) -> anyhow::Result<String> {
    let name = value.trim().to_ascii_lowercase();
    let name = match name.as_str() {
        // sway spells these two differently, and swaybg is what most people
        // set a wallpaper with before they set one here.
        "cover" => "fill",
        "contain" => "fit",
        other => other,
    };
    if WALLPAPER_MODES.contains(&name) {
        return Ok(name.to_owned());
    }
    Err(anyhow::anyhow!(
        "wallpaper mode {value:?} is not one of {}",
        WALLPAPER_MODES.join(", ")
    ))
}

/// How many bits per colour channel a scanout buffer carries.
///
/// Depth rather than a fourcc, because the byte order is not the interesting
/// part: a display takes several orderings of the same depth and the driver
/// picks between them, while the choice between eight and ten bits is the one
/// that shows on screen — banding in a gradient on one side, and on the other
/// a wider buffer that some displays will not scan out at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// Ten bits first, eight if the display will not take it. The default.
    Auto,
    /// Eight bits, whatever the display can do.
    Eight,
    /// Ten bits and nothing else. An output whose plane will not take a
    /// ten-bit format does not come up at all, which is the point — it is how
    /// "did I get ten bits" is answered without reading a plane dump.
    Ten,
}

/// What `pixel_format`, `--pixel-format` and `$VIEWPORT_PIXEL_FORMAT` accept.
///
/// The bit depth is what is named, in every form somebody types it: `10`,
/// `10bit`, `10-bit`. `auto` and an empty value are the default, so unsetting
/// the variable and setting it to nothing agree.
pub fn parse_pixel_format(value: &str) -> anyhow::Result<PixelFormat> {
    let trimmed = value.trim().to_ascii_lowercase();
    // Only where a depth is left in front of it, or a bare "bit" would strip
    // down to the empty string and be read as the default.
    let depth = match trimmed
        .strip_suffix("bit")
        .map(|d| d.trim_end_matches(['-', '_', ' ']))
    {
        Some(depth) if !depth.is_empty() => depth,
        _ => trimmed.as_str(),
    };
    match depth {
        "" | "auto" | "default" => Ok(PixelFormat::Auto),
        "8" => Ok(PixelFormat::Eight),
        "10" => Ok(PixelFormat::Ten),
        _ => Err(anyhow::anyhow!(
            "pixel format {value:?} is not one of 8, 10 or auto"
        )),
    }
}

/// Whether the on-screen keyboard is allowed to raise itself, and if not,
/// whether `Mod4+Shift+k` still can.
///
/// A boolean is not enough because it conflates two different complaints. "I
/// have a hardware keyboard, stop putting a second one on top of my search
/// box" and "I never want to see this thing" are not the same request: the
/// first still wants the on-screen keyboard reachable by hand for the rare
/// window that never takes hardware input in the first place — a login
/// prompt XWayland drew before the session's own input method was up, most
/// often — while the second is asking for it gone entirely, chord included.
/// A plain on/off can only grant one of those two without also granting the
/// other, so this is three values instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OskMode {
    /// Raises itself the way it always has, and `Mod4+Shift+k` still works
    /// on top of that. The default — but see `ViewportState::sync_osk_wanted`
    /// in `input.rs` for the one change made to what "raises itself" means:
    /// only once the seat has actually seen a touch device, rather than on
    /// every text-input a focused client enables. A desktop with a keyboard
    /// and a mouse and nothing else never sees one, so this behaves like
    /// [`OskMode::Manual`] there without a config file having to say so.
    Auto,
    /// Never raises itself; only `Mod4+Shift+k`, or the keyboard's own hide
    /// button once it is up, brings it up or down. For a desk that has a
    /// keyboard already and wants the on-screen one kept out of the way but
    /// not out of reach.
    Manual,
    /// Neither raises itself nor answers the chord. `Mod4+Shift+k` is bound
    /// to nothing else while this is set, so pressing it does exactly
    /// nothing — a deliberate consequence of asking for the keyboard gone
    /// entirely, not a bug in the keymap. See `commands.js`'s `osk` case in
    /// the shell for where that is enforced, since the chord reaches the
    /// shell as a plain `shell osk` action the compositor does not itself
    /// interpret.
    Off,
}

impl OskMode {
    /// The spelling this reads back as, over the config event to the shell —
    /// kept next to [`parse_osk_mode`] so the two stay in agreement about
    /// what the three values are called.
    pub fn as_str(self) -> &'static str {
        match self {
            OskMode::Auto => "auto",
            OskMode::Manual => "manual",
            OskMode::Off => "off",
        }
    }
}

/// What `osk` in the config file accepts.
///
/// Unknown is an error rather than a silent `"auto"`, for the same reason
/// [`parse_pixel_format`] refuses a bad value instead of guessing: a typo
/// here would leave someone's hardware-keyboard desktop with the popup they
/// were trying to turn off, and no indication the setting was ignored.
pub fn parse_osk_mode(value: &str) -> anyhow::Result<OskMode> {
    match value.trim().to_ascii_lowercase().as_str() {
        "auto" => Ok(OskMode::Auto),
        "manual" => Ok(OskMode::Manual),
        "off" => Ok(OskMode::Off),
        _ => Err(anyhow::anyhow!(
            "osk mode {value:?} is not one of auto, manual or off"
        )),
    }
}

/// Percent-encode everything a path may contain that a URI may not.
///
/// `/` is kept, because it is the path separator rather than data. Everything
/// outside the unreserved set goes, which over-encodes a little — `@` and `+`
/// would have been legal — and is never wrong.
fn encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Undo percent-encoding, so an encoded path can be checked on disk.
///
/// Anything that is not a well-formed escape is passed through, because this
/// only feeds an existence check — a wrong guess there produces a worse error
/// message, not a worse outcome.
pub(crate) fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(byte) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// What `xwayland.scale` was written as: a name, or a number.
///
/// Two shapes for one key because the two useful answers are different kinds
/// of thing — `"auto"` is a policy and `2` is a decision, and a desk that has
/// picked a number does not want it re-derived from the monitors the next
/// time one is plugged in. [`parse_xwayland_scale`] turns either into a
/// [`XwaylandScale`]; the untagged enum only gets it out of JSON without the
/// file having to say which of the two it meant.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum XwaylandScaleSetting {
    Name(String),
    Factor(f64),
}

/// What `xwayland.scale` means once it has been read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum XwaylandScale {
    /// X11 clients are left at 1x: their buffers are logical pixels and the
    /// compositor magnifies them onto a HiDPI panel, which is blurry and is
    /// the right *size*. The default, and the only setting under which an X11
    /// application that cannot scale itself still comes out the size it
    /// should be — see the Xwayland section of `docs/protocols.md` for why
    /// that is the trade and not an oversight.
    #[default]
    Off,
    /// Take the number from the monitors — [`pick_xwayland_scale`].
    Auto,
    /// The number the file gave, whatever the monitors say.
    Fixed(u32),
}

impl XwaylandScale {
    /// The spelling this reads back as, for a log line that has to say what
    /// was asked for as well as what came of it.
    pub fn as_str(self) -> std::borrow::Cow<'static, str> {
        match self {
            XwaylandScale::Off => "off".into(),
            XwaylandScale::Auto => "auto".into(),
            XwaylandScale::Fixed(n) => n.to_string().into(),
        }
    }
}

/// The `xwayland` block.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct XwaylandConfig {
    /// `"off"` (the default), `"auto"`, or a whole number — see
    /// [`parse_xwayland_scale`] and [`XwaylandScale`].
    ///
    /// Read once, when Xwayland is started. A reload moves the value here and
    /// nothing on screen with it: the X screen's size in X pixels is a
    /// function of this, X clients read it once at connect, and resizing the
    /// root window under a running xterm is not a thing X11 lets a window
    /// manager do gracefully.
    pub scale: Option<XwaylandScaleSetting>,
}

/// The largest scale the key will accept.
///
/// Not a hardware limit — an arbitrary one, so that a typo (`"scale": 200`,
/// meaning percent) does not hand Xwayland a root window two hundred times
/// the size of the desk and an allocation to match.
pub const MAX_XWAYLAND_SCALE: u32 = 8;

/// The scale X11 clients should be told to draw at.
///
/// Pure, and deliberately so: which number falls out of a set of monitors is
/// the whole of the policy, and a policy that can only be exercised by
/// plugging a 4K panel in is a policy nobody ever checks. `scales` is every
/// monitor's scale — the live ones *and* whatever the config file asked for,
/// because on the first start the file has been read and an output may not
/// have arrived yet, and because a monitor that is switched off still has a
/// scale somebody chose for it.
///
/// A mixed-DPI desk has no right answer here, and this does not pretend to
/// one. There is a single X screen behind every monitor, so there is a single
/// number: the largest. That makes the sharpest panel right and leaves an X
/// window on the 1x monitor drawing four times the pixels it needs into the
/// same rectangle — wasteful, and the correct size on both, which is the part
/// that matters. Picking the smallest instead would keep the blur on exactly
/// the panel that was the reason to turn this on.
///
/// Fractional scales round rather than truncate. The only wire this has to
/// the toolkits is an integer window-scaling factor, so 1.5 is either 1 or 2,
/// and 2 is the one that leaves text sharp.
pub fn pick_xwayland_scale(setting: XwaylandScale, scales: impl IntoIterator<Item = f64>) -> u32 {
    match setting {
        XwaylandScale::Off => 1,
        XwaylandScale::Fixed(n) => n,
        XwaylandScale::Auto => scales
            .into_iter()
            .filter(|scale| scale.is_finite())
            .map(|scale| (scale.round() as i64).clamp(1, MAX_XWAYLAND_SCALE as i64) as u32)
            .max()
            .unwrap_or(1),
    }
}

/// What `xwayland.scale` in the config file accepts.
///
/// `"off"` and `"auto"` by name, or a whole number. Refused rather than
/// guessed at, the way [`parse_osk_mode`] refuses: this key decides how every
/// X11 window on the desk is sized, and a value quietly ignored reads as
/// "Viewport cannot do this" rather than "that is not a value".
///
/// A fractional number is refused outright rather than rounded. The scale
/// reaches the toolkits as an integer window-scaling factor — X11 has no wire
/// for anything else — so `1.5` would silently become `2`, and a setting that
/// means something other than what it says is worse than one that will not
/// load.
pub fn parse_xwayland_scale(value: &XwaylandScaleSetting) -> anyhow::Result<XwaylandScale> {
    match value {
        XwaylandScaleSetting::Name(name) => match name.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "1" => Ok(XwaylandScale::Off),
            "auto" => Ok(XwaylandScale::Auto),
            _ => Err(anyhow::anyhow!(
                "xwayland scale {name:?} is not off, auto or a whole number"
            )),
        },
        XwaylandScaleSetting::Factor(factor) => {
            if !factor.is_finite() || factor.fract() != 0.0 {
                return Err(anyhow::anyhow!(
                    "xwayland scale {factor} is not a whole number, and X11 toolkits take \
                     an integer window scale and nothing else"
                ));
            }
            let whole = *factor as i64;
            if !(1..=MAX_XWAYLAND_SCALE as i64).contains(&whole) {
                return Err(anyhow::anyhow!(
                    "xwayland scale {whole} is outside 1..={MAX_XWAYLAND_SCALE}"
                ));
            }
            // 1 is off rather than a scale of one, so that the whole of the
            // scaling apparatus — the client scale, the X settings, the
            // doubled cursor — is never set up to multiply by nothing.
            if whole == 1 {
                Ok(XwaylandScale::Off)
            } else {
                Ok(XwaylandScale::Fixed(whole as u32))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pixel_format_names_its_bit_depth_in_any_form_somebody_types_it() {
        assert_eq!(parse_pixel_format("8").unwrap(), PixelFormat::Eight);
        assert_eq!(parse_pixel_format("10").unwrap(), PixelFormat::Ten);
        assert_eq!(parse_pixel_format(" 10bit ").unwrap(), PixelFormat::Ten);
        assert_eq!(parse_pixel_format("10-BIT").unwrap(), PixelFormat::Ten);
        assert_eq!(parse_pixel_format("8_bit").unwrap(), PixelFormat::Eight);
    }

    #[test]
    fn an_unset_pixel_format_and_an_empty_one_agree() {
        // The environment variable is read with unwrap_or_default, so "" is
        // what "not set at all" looks like by the time it is parsed. If that
        // were an error, every session without the variable would log a
        // warning about a value nobody wrote.
        assert_eq!(parse_pixel_format("").unwrap(), PixelFormat::Auto);
        assert_eq!(parse_pixel_format("  ").unwrap(), PixelFormat::Auto);
        assert_eq!(parse_pixel_format("auto").unwrap(), PixelFormat::Auto);
    }

    #[test]
    fn a_pixel_format_that_is_not_a_depth_is_reported_not_ignored() {
        // Silently defaulting would leave someone chasing banding with the
        // setting meant to fix it sitting in their config doing nothing.
        assert!(parse_pixel_format("16").is_err());
        assert!(parse_pixel_format("argb8888").is_err());
        assert!(parse_pixel_format("bit").is_err());
    }

    #[test]
    fn an_osk_mode_names_the_three_things_someone_can_mean() {
        assert_eq!(parse_osk_mode("auto").unwrap(), OskMode::Auto);
        assert_eq!(parse_osk_mode("manual").unwrap(), OskMode::Manual);
        assert_eq!(parse_osk_mode("off").unwrap(), OskMode::Off);
        // Trimmed and case-folded, the same forgiveness pixel_format gets —
        // a config file is typed by hand and "Off" is as clear as "off".
        assert_eq!(parse_osk_mode(" Off ").unwrap(), OskMode::Off);
        assert_eq!(parse_osk_mode("MANUAL").unwrap(), OskMode::Manual);
    }

    #[test]
    fn an_osk_mode_round_trips_through_its_own_spelling() {
        // as_str is what state.rs hands the shell back over the config event;
        // it has to agree with what parse_osk_mode accepted or a reload could
        // display a mode the file never actually asked for.
        for mode in [OskMode::Auto, OskMode::Manual, OskMode::Off] {
            assert_eq!(parse_osk_mode(mode.as_str()).unwrap(), mode);
        }
    }

    #[test]
    fn an_osk_mode_that_is_not_one_of_the_three_is_reported_not_ignored() {
        // A typo silently falling back to auto would leave the keyboard
        // popping up on exactly the desk that configured it to stop.
        assert!(parse_osk_mode("").is_err());
        assert!(parse_osk_mode("never").is_err());
        assert!(parse_osk_mode("true").is_err());
    }

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
        for bad in [
            "",
            "2560",
            "2560x",
            "x1440",
            "2560x1440@",
            "0x1440",
            "2560x1440@0",
        ] {
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
        let file: File = serde_json::from_str(r#"{"terminal":"foot","invented_later":true}"#)
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
                "cursor": {"theme":"Adwaita","size":32,"hide_after_ms":1500},
                "idle": {"lock_after":300,"lock_command":"swaylock"},
                "magnify": {"step":0.25,"max":6.0},
                "outputs": {"DP-1":{"mode":"2560x1440@120","x":0,"hdr":true}}
            }"#,
        )
        .expect("should parse");
        assert_eq!(file.keyboard.layout.as_deref(), Some("us"));
        assert_eq!(file.keyboard.repeat_rate, Some(25));
        assert_eq!(file.cursor.size, Some(32));
        assert_eq!(file.cursor.hide_after_ms, Some(1500));
        assert_eq!(file.idle.lock_after, Some(300));
        assert_eq!(file.magnify.step, Some(0.25));
        assert_eq!(file.magnify.max, Some(6.0));
        let dp1 = file.outputs.get("DP-1").expect("DP-1");
        assert_eq!(dp1.mode.as_deref(), Some("2560x1440@120"));
        assert_eq!(dp1.hdr, Some(true));
        // Absent within a block is still absent, not zero.
        assert_eq!(dp1.y, None);
    }

    #[test]
    fn bar_widgets_parse_by_type() {
        // Each widget names its kind and then the options that kind takes. The
        // default bar does not change — these only add to it.
        let file: File = serde_json::from_str(
            r#"{
                "bar_widgets": [
                    {"type":"disk","path":"/home"},
                    {"type":"disk"},
                    {"type":"weather","location":"New York"},
                    {"type":"volume"},
                    {"type":"mic"},
                    {"type":"battery"}
                ]
            }"#,
        )
        .expect("should parse");
        assert_eq!(file.bar_widgets.len(), 6);
        assert_eq!(
            file.bar_widgets[0],
            BarWidgetConfig::Disk {
                path: Some("/home".into())
            }
        );
        // A path is optional and defaults to nothing here; the sampler treats
        // None as the root mount.
        assert_eq!(file.bar_widgets[1], BarWidgetConfig::Disk { path: None });
        assert_eq!(
            file.bar_widgets[2],
            BarWidgetConfig::Weather {
                location: Some("New York".into())
            }
        );
        assert_eq!(file.bar_widgets[3], BarWidgetConfig::Volume);
        assert_eq!(file.bar_widgets[4], BarWidgetConfig::Mic);
        assert_eq!(file.bar_widgets[5], BarWidgetConfig::Battery);
    }

    #[test]
    fn clock_block_parses_and_absence_is_the_engine_s_own_locale() {
        // Every field optional and independent: the common case is naming one
        // of them. A `clock` block that named a locale and nothing else used
        // to be the only way to get a German month, and there was no way at
        // all.
        let file: File = serde_json::from_str(
            r#"{"clock":{"locale":"de-DE","hour12":false,"format":"%a %d %b %H:%M"}}"#,
        )
        .expect("should parse");
        assert_eq!(file.clock.locale.as_deref(), Some("de-DE"));
        assert_eq!(file.clock.hour12, Some(false));
        assert_eq!(file.clock.format.as_deref(), Some("%a %d %b %H:%M"));

        let file: File =
            serde_json::from_str(r#"{"clock":{"hour12":true}}"#).expect("should parse");
        assert_eq!(file.clock.hour12, Some(true));
        // Not "en-US", and not any other tag this side could invent: an absent
        // locale is the shell asking the engine what the session's is.
        assert_eq!(file.clock.locale, None);

        // And an absent block is distinguishable from an empty one only in
        // that neither says anything, which is what apply_config tests for
        // before it forwards a thing.
        let file: File = serde_json::from_str("{}").expect("should parse");
        assert_eq!(file.clock, ClockConfig::default());
    }

    #[test]
    fn lid_parses_as_a_name() {
        let file: File = serde_json::from_str(r#"{"lid":"suspend"}"#).expect("should parse");
        assert_eq!(file.lid.as_deref(), Some("suspend"));
        let file: File = serde_json::from_str("{}").expect("should parse");
        assert_eq!(file.lid, None);
    }

    #[test]
    fn bar_widgets_absent_is_the_default_bar() {
        // No widgets means the default bar, exactly as if the key were not
        // there at all.
        let file: File = serde_json::from_str("{}").expect("should parse");
        assert!(file.bar_widgets.is_empty());
        assert!(file.bar_items.is_none());
    }

    #[test]
    fn bar_items_parse_mixed_modules_and_widgets() {
        // A bar_items override names built-in modules as bare strings and
        // widgets as objects, in whatever order the user wants them drawn.
        let file: File = serde_json::from_str(
            r#"{
                "bar_items": [
                    "net",
                    {"type":"disk","path":"/games"},
                    "clock",
                    {"type":"weather","location":"Pickering, ON, Canada"}
                ]
            }"#,
        )
        .expect("should parse");
        let items = file.bar_items.as_ref().unwrap();
        assert_eq!(items.len(), 4);
        assert_eq!(items[0], BarItemConfig::Module("net".into()));
        assert_eq!(
            items[1],
            BarItemConfig::Widget(BarWidgetConfig::Disk {
                path: Some("/games".into())
            })
        );
        assert_eq!(items[2], BarItemConfig::Module("clock".into()));
        assert_eq!(
            items[3],
            BarItemConfig::Widget(BarWidgetConfig::Weather {
                location: Some("Pickering, ON, Canada".into())
            })
        );
    }

    #[test]
    fn bar_items_empty_still_overrides() {
        // Present but empty: the user asked for no right-side contents at all.
        let file: File = serde_json::from_str(r#"{"bar_items":[]}"#).expect("should parse");
        assert_eq!(file.bar_items.as_ref().unwrap().len(), 0);
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
    fn gaps_block_parses() {
        let set: File = serde_json::from_str(r#"{"gaps":{"inner":15}}"#).expect("should parse");
        assert_eq!(set.gaps.inner, Some(15));
        // Absent within the block, and an absent block, are both absent.
        let empty_block: File = serde_json::from_str(r#"{"gaps":{}}"#).expect("should parse");
        assert_eq!(empty_block.gaps.inner, None);
        let absent: File = serde_json::from_str("{}").expect("should parse");
        assert_eq!(absent.gaps, GapsConfig::default());
    }

    #[test]
    fn border_block_parses() {
        let set: File = serde_json::from_str(r#"{"border":{"radius":12,"width":3,"smart":true}}"#)
            .expect("should parse");
        assert_eq!(set.border.radius, Some(12));
        assert_eq!(set.border.width, Some(3));
        assert_eq!(set.border.smart, Some(true));
        // Absent is not false: it follows `gaps.smart`, and only the shell can
        // tell those apart, so it has to reach the wire as absent.
        let quiet: File = serde_json::from_str(r#"{"border":{}}"#).expect("should parse");
        assert_eq!(quiet.border.smart, None);
        // Zero is a decision — square corners — and not an absent field.
        let square: File =
            serde_json::from_str(r#"{"border":{"radius":0}}"#).expect("should parse");
        assert_eq!(square.border.radius, Some(0));
        assert_ne!(square.border, BorderConfig::default());
        let absent: File = serde_json::from_str("{}").expect("should parse");
        assert_eq!(absent.border, BorderConfig::default());
    }

    #[test]
    fn the_wallpaper_keys_parse() {
        let file: File =
            serde_json::from_str(r#"{"wallpaper":"~/wall.png","wallpaper_mode":"fit"}"#)
                .expect("should parse");
        assert_eq!(file.wallpaper.as_deref(), Some("~/wall.png"));
        assert_eq!(file.wallpaper_mode.as_deref(), Some("fit"));
        // Absent is the shell's own background, and an empty string is how a
        // file asks for it back — the two have to stay distinguishable, or a
        // reload could not remove a wallpaper it had set.
        let absent: File = serde_json::from_str("{}").expect("should parse");
        assert_eq!(absent.wallpaper, None);
        let cleared: File = serde_json::from_str(r#"{"wallpaper":""}"#).expect("should parse");
        assert_eq!(cleared.wallpaper.as_deref(), Some(""));
    }

    #[test]
    fn the_wallpaper_modes_are_the_ones_stylix_writes() {
        // stylix's `imageScalingMode` is handed across unchanged by a themed
        // NixOS session, so these five names are a compatibility promise and
        // not a vocabulary of ours.
        for mode in ["stretch", "fill", "fit", "center", "tile"] {
            assert_eq!(parse_wallpaper_mode(mode).unwrap(), mode);
        }
        // Case and spacing are what a config file has, not a mistake.
        assert_eq!(parse_wallpaper_mode(" Fill ").unwrap(), "fill");
        // And sway's two names for the same fittings, because swaybg is what
        // most people set a wallpaper with before they set one here.
        assert_eq!(parse_wallpaper_mode("cover").unwrap(), "fill");
        assert_eq!(parse_wallpaper_mode("contain").unwrap(), "fit");
    }

    #[test]
    fn an_unknown_wallpaper_mode_is_refused_rather_than_filled() {
        // The mode is the difference between a photo and a stretched photo, so
        // a typo silently becoming `fill` is a setting that looks ignored.
        let error = parse_wallpaper_mode("zoom")
            .expect_err("not a mode")
            .to_string();
        assert!(error.contains("fill"), "{error}");
    }

    #[test]
    fn a_wallpaper_path_becomes_a_url_the_page_can_load() {
        let dir = std::env::temp_dir().join("viewport wallpaper test");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let file = dir.join("wall paper.png");
        std::fs::write(&file, "not really a png").expect("write");

        let url = wallpaper_value(&file.to_string_lossy(), "--wallpaper").expect("exists");
        assert!(url.starts_with("file:///"), "{url}");
        // The same encoding `--url` needs, and for the same reason: a picture
        // in a directory with a space in it is not a URI until the space is.
        assert!(!url.contains(' '), "a space survived: {url}");
        // Both spaces in the file's own name, and however many the temporary
        // directory above it happens to carry.
        assert!(url.ends_with("/wall%20paper.png"), "{url}");
        assert!(url.contains("wallpaper%20test"), "{url}");

        // A missing one is named, and named as whatever asked for it — the
        // point of the message is that somebody can find what they typed.
        let error = wallpaper_value("/nowhere/wall.png", "wallpaper").expect_err("missing");
        assert!(error.to_string().starts_with("wallpaper:"), "{error}");

        // And `~`, which a config file is entitled to write because it is not
        // a shell and nothing else in the process would expand it.
        let home = std::env::var("HOME").expect("a home directory");
        let in_home = std::path::Path::new(&home).join("viewport-wallpaper-test.png");
        std::fs::write(&in_home, "not really a png").expect("write");
        let url = wallpaper_value("~/viewport-wallpaper-test.png", "wallpaper").expect("exists");
        assert_eq!(
            url,
            format!("file://{}", in_home.display()),
            "a leading ~ was not expanded"
        );
        let _ = std::fs::remove_file(&in_home);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_colour_or_a_gradient_is_a_wallpaper_too() {
        // Not every wallpaper is a photograph. These are passed through
        // untouched — resolving them as paths would refuse what was asked.
        for css in [
            "#1a1b26",
            "rgb(26, 27, 38)",
            "transparent",
            "linear-gradient(#1a1b26, #24283b)",
            "radial-gradient(circle at 20% 0, #24283b, #1a1b26)",
            // The spelling somebody arriving from a CSS background writes.
            "url(/pic/wall.png)",
        ] {
            assert_eq!(
                wallpaper_value(css, "wallpaper").expect("a css value"),
                css,
                "{css} was not passed through"
            );
        }
        // And whitespace around one, which is what a config file has.
        assert_eq!(
            wallpaper_value("  #1a1b26  ", "wallpaper").unwrap(),
            "#1a1b26"
        );
    }

    #[test]
    fn a_path_that_ends_in_a_bracket_is_still_a_path() {
        // The rule is `name(...)` with a CSS identifier in front, so a holiday
        // picture with a number after it is not mistaken for a function.
        assert!(!looks_like_css("/home/me/holiday (1)"));
        assert!(!looks_like_css("wall.png"));
        assert!(!looks_like_css("~/Pictures/wall.png"));
        assert!(looks_like_css("rgb(0,0,0)"));
        assert!(looks_like_css("#fff"));
    }

    #[test]
    fn a_colour_written_by_name_says_how_to_write_it() {
        // `"wallpaper": "black"` is a relative path as far as this can tell,
        // and "black does not exist" is a message about the wrong thing.
        // Named colours are not accepted, because a relative path is a real
        // way to name a picture and there is no telling the two apart.
        let error = wallpaper_value("black", "wallpaper")
            .expect_err("not a file")
            .to_string();
        assert!(error.contains("#rrggbb"), "{error}");
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

    #[test]
    fn a_space_in_the_path_is_encoded() {
        // The case this exists for: a package unpacked under "Telegram
        // Desktop". Unencoded, the URL is not a URI at all and the shell that
        // loads is not the one that was named.
        let dir = std::env::temp_dir().join("viewport url test/shell");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let file = dir.join("index.html");
        std::fs::write(&file, "<html></html>").expect("write");

        let url = shell_url(&format!("file://{}", file.display())).expect("a file that exists");
        assert!(!url.contains(' '), "a space survived: {url}");
        assert!(url.contains("%20"), "the space was not encoded: {url}");

        // And the same file named as a plain path, which is what someone types.
        let from_path = shell_url(&file.to_string_lossy()).expect("a path that exists");
        assert!(from_path.starts_with("file:///"), "{from_path}");
        assert!(from_path.contains("%20"), "{from_path}");

        let _ = std::fs::remove_dir_all(std::env::temp_dir().join("viewport url test"));
    }

    #[test]
    fn a_missing_file_is_named_rather_than_silently_ignored() {
        // Falling back to the default shell here is what made a typo look like
        // a compositor that ignores --url.
        let error = shell_url("/definitely/not/here/index.html").expect_err("missing");
        assert!(error.to_string().contains("does not exist"), "{error}");

        let error = shell_url("file:///definitely/not/here/index.html").expect_err("missing");
        assert!(error.to_string().contains("does not exist"), "{error}");
    }

    #[test]
    fn an_already_encoded_url_is_not_encoded_twice() {
        // %20 becoming %2520 would break a URL that was already correct.
        let dir = std::env::temp_dir().join("viewport url test2/a b");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let file = dir.join("index.html");
        std::fs::write(&file, "x").expect("write");

        let encoded = format!("file://{}", file.display().to_string().replace(' ', "%20"));
        let url = shell_url(&encoded).expect("exists");
        assert!(!url.contains("%2520"), "double-encoded: {url}");
        assert_eq!(url, encoded);

        let _ = std::fs::remove_dir_all(std::env::temp_dir().join("viewport url test2"));
    }

    #[test]
    fn a_remote_url_is_left_alone_and_not_checked_on_disk() {
        // An http shell that is not up yet is a waiting game, not a mistake.
        assert_eq!(
            shell_url("http://localhost:8000/index.html").expect("remote"),
            "http://localhost:8000/index.html"
        );
        assert!(shell_url("").is_err(), "nothing to load");
    }

    #[test]
    fn an_unknown_tiling_mode_is_refused_rather_than_passed_on() {
        // The compositor has no layout, so an unknown name would reach the
        // shell, match no arrangement, and leave the tree manual with nothing
        // anywhere saying why.
        let file: File = serde_json::from_str(r#"{"tiling_mode": "fibonacci"}"#).expect("parses");
        assert_eq!(file.tiling_mode.as_deref(), Some("fibonacci"));
        // Rejection happens in apply_config, which is what the log line is
        // attached to; the file itself carries whatever was written.
    }

    #[test]
    fn the_tiling_modes_round_trip() {
        for mode in ["manual", "master-stack", "spiral", "bsp", "grid"] {
            let file: File =
                serde_json::from_str(&format!(r#"{{"tiling_mode": "{mode}"}}"#)).expect("parses");
            assert_eq!(file.tiling_mode.as_deref(), Some(mode));
        }
    }

    #[test]
    fn an_xwayland_scale_is_off_auto_or_a_whole_number() {
        let name = |text: &str| XwaylandScaleSetting::Name(text.to_owned());
        assert_eq!(
            parse_xwayland_scale(&name("off")).unwrap(),
            XwaylandScale::Off
        );
        assert_eq!(
            parse_xwayland_scale(&name(" AUTO ")).unwrap(),
            XwaylandScale::Auto
        );
        // A file may spell the number as a string; both mean the same, and
        // one of the two working would be a bug report about JSON types.
        assert_eq!(
            parse_xwayland_scale(&name("1")).unwrap(),
            XwaylandScale::Off
        );
        assert_eq!(
            parse_xwayland_scale(&XwaylandScaleSetting::Factor(1.0)).unwrap(),
            XwaylandScale::Off
        );
        assert_eq!(
            parse_xwayland_scale(&XwaylandScaleSetting::Factor(2.0)).unwrap(),
            XwaylandScale::Fixed(2)
        );
        // Refused, not rounded: see parse_xwayland_scale.
        assert!(parse_xwayland_scale(&XwaylandScaleSetting::Factor(1.5)).is_err());
        assert!(parse_xwayland_scale(&XwaylandScaleSetting::Factor(0.0)).is_err());
        assert!(parse_xwayland_scale(&XwaylandScaleSetting::Factor(200.0)).is_err());
        assert!(parse_xwayland_scale(&name("2x")).is_err());
    }

    #[test]
    fn the_xwayland_block_parses_either_shape() {
        let file: File = serde_json::from_str(r#"{"xwayland": {"scale": 2}}"#).expect("parses");
        assert_eq!(file.xwayland.scale, Some(XwaylandScaleSetting::Factor(2.0)));
        let file: File =
            serde_json::from_str(r#"{"xwayland": {"scale": "auto"}}"#).expect("parses");
        assert_eq!(
            file.xwayland.scale,
            Some(XwaylandScaleSetting::Name("auto".to_owned()))
        );
        // Absent is absent, which is what keeps a file that says nothing
        // about X11 from changing what X11 clients see.
        let file: File = serde_json::from_str("{}").expect("parses");
        assert_eq!(file.xwayland.scale, None);
    }

    #[test]
    fn the_xwayland_scale_comes_from_the_sharpest_monitor() {
        // Off is off no matter what is plugged in.
        assert_eq!(pick_xwayland_scale(XwaylandScale::Off, [2.0, 3.0]), 1);
        // A fixed number ignores the monitors entirely — that is the point of
        // spelling one out rather than asking for auto.
        assert_eq!(pick_xwayland_scale(XwaylandScale::Fixed(3), [1.0]), 3);

        assert_eq!(pick_xwayland_scale(XwaylandScale::Auto, [1.0]), 1);
        assert_eq!(pick_xwayland_scale(XwaylandScale::Auto, [2.0]), 2);
        // The mixed-DPI desk, which is the case with no right answer: a 2x
        // laptop panel beside a 1x monitor picks 2, so the panel is sharp and
        // the monitor merely draws more pixels than it needs.
        assert_eq!(pick_xwayland_scale(XwaylandScale::Auto, [1.0, 2.0]), 2);
        assert_eq!(pick_xwayland_scale(XwaylandScale::Auto, [2.0, 1.0]), 2);
        assert_eq!(pick_xwayland_scale(XwaylandScale::Auto, [3.0, 1.0, 2.0]), 3);
        // Fractional scales round to the integer the toolkits can carry.
        assert_eq!(pick_xwayland_scale(XwaylandScale::Auto, [1.5]), 2);
        assert_eq!(pick_xwayland_scale(XwaylandScale::Auto, [1.25]), 1);
        assert_eq!(pick_xwayland_scale(XwaylandScale::Auto, [1.25, 1.75]), 2);
        // No monitors at all — headless, or a session started before any
        // connector came up — is 1 rather than a panic or a zero.
        assert_eq!(pick_xwayland_scale(XwaylandScale::Auto, []), 1);
        // Nonsense from a config file that got a scale of zero past the
        // output parser: never below 1, never above the cap.
        assert_eq!(pick_xwayland_scale(XwaylandScale::Auto, [0.0]), 1);
        assert_eq!(pick_xwayland_scale(XwaylandScale::Auto, [f64::NAN, 2.0]), 2);
        assert_eq!(pick_xwayland_scale(XwaylandScale::Auto, [1000.0]), 8);
    }

    #[test]
    fn the_layout_models_round_trip() {
        // The same five names `--layout` takes and apply_config checks
        // against. A model added to one and not the others is a config key
        // that parses, is rejected, and leaves the keymap built for something
        // else.
        for layout in ["tiling", "scrolling", "solar", "matrix", "canvas"] {
            let file: File =
                serde_json::from_str(&format!(r#"{{"layout": "{layout}"}}"#)).expect("parses");
            assert_eq!(file.layout.as_deref(), Some(layout));
        }
    }
}
