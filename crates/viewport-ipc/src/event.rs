// SPDX-License-Identifier: MIT
//
// Outbound messages: compositor to shell. Ports the viewport_ipc_notify_*
// family in src/ipc.c.
//
// Key order does not matter here — the shell runs these through JSON.parse —
// but key names, types and the empty-string-for-null convention do. The C build
// never emits JSON null: an absent make/model/serial goes out as "" so the
// shell can concatenate without guarding (`src/ipc.c:704`).

use serde::{Deserialize, Serialize};

use crate::geometry::Transform;

/// A message from the compositor to the shell.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Event {
    /// A window appeared. Also re-sent for every mapped window in reply to
    /// `view.query`, with `replay` set — that is how a reloading shell rebuilds
    /// its tree without the windows appearing to be new.
    #[serde(rename = "view.added")]
    ViewAdded(ViewAdded),

    #[serde(rename = "view.removed")]
    ViewRemoved { id: u32 },

    #[serde(rename = "view.props")]
    ViewProps {
        id: u32,
        title: String,
        app_id: String,
        /// What the window says it looks like in a list, from
        /// `xdg-toplevel-icon`: a name to look up in the icon theme. Absent
        /// when the client has not said, which is most of them — omitted from
        /// the wire entirely rather than sent as null, so a shell written
        /// against the older message sees exactly what it saw before.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        icon: Option<String>,
    },

    /// The window this one is a dialog of has changed, or arrived late.
    ///
    /// `view.added` carries the parent when it is known by then, and for a
    /// dialog opened by the application itself it is. It is not for a dialog
    /// that comes from somewhere else — a file chooser is the portal's window,
    /// in another process, and the parent reaches the compositor over
    /// xdg-foreign after an export and an import have gone round. By then the
    /// window has mapped and been announced with no parent at all, which is a
    /// dialog the shell cannot place next to anything.
    ///
    /// `parent` is absent when the window has been unparented, which is the
    /// same message with the same meaning: this is who it belongs to now.
    #[serde(rename = "view.parent")]
    ViewParent {
        id: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<u32>,
    },

    /// The size the client was actually configured with, when it is not the
    /// size the shell asked for.
    ///
    /// A client configured below its minimum is entitled to ignore it and
    /// commit whatever size it likes, so the compositor raises the configure to
    /// that minimum rather than sending a request it knows will be refused.
    /// Silently, until now — which left the shell holding a rectangle for a
    /// window that is a different size, and every sum built on that rectangle
    /// wrong by the difference. Centring a dialog on its parent is one such
    /// sum, and is out by half of it.
    ///
    /// Sent only when the two disagree, and only when the answer changes, so a
    /// shell resending the same rectangle sixty times a second is told once.
    /// A minimum is not fixed for the life of a window: a client may raise it
    /// long after it mapped, which is why this cannot be answered from
    /// `view.added` alone.
    #[serde(rename = "view.configured")]
    ViewConfigured { id: u32, width: i32, height: i32 },

    #[serde(rename = "view.focused")]
    ViewFocused { id: u32 },

    /// A client outside the shell asked for something to happen to a
    /// workspace, through `ext-workspace-v1`.
    ///
    /// Forwarded rather than acted on: the workspaces are the shell's, so a
    /// request to switch to one is a request *of the shell*, which does it and
    /// says so with the next `workspace.list`. A shell that ignores these is a
    /// desktop where an external bar can list workspaces and not switch them,
    /// which is the honest result of ignoring them.
    ///
    /// `action` is one of `activate`, `deactivate`, `assign`, `remove` or
    /// `create`. `id` names the workspace for all but `create`, which carries
    /// `name` and the `output` to make it on.
    #[serde(rename = "workspace.request")]
    WorkspaceRequest {
        action: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<String>,
    },

    #[serde(rename = "config")]
    Config(Config),

    /// The logo modifier went down or came up, so the shell can show its
    /// overlay while it is held.
    #[serde(rename = "modifiers")]
    Modifiers { logo: bool },

    /// The stored layout, handed back when the shell asks for it. Sent on
    /// request rather than on connect: a shell that has not finished loading
    /// cannot do anything with it.
    #[serde(rename = "session.restore")]
    SessionRestore { state: String },

    #[serde(rename = "notification.add")]
    NotificationAdd(Notification),

    #[serde(rename = "notification.close")]
    NotificationClose { id: u32 },

    /// The system tray, whole, every time any part of it changes.
    ///
    /// Not a stream of add, change and remove messages. A tray is a handful of
    /// items and every one of them is redrawn from a snapshot in a single
    /// pass, so reconciling three message kinds in the shell would be work
    /// spent to save a few hundred bytes on a message that arrives when an
    /// application starts or its icon changes — not on a timer, and not per
    /// frame.
    #[serde(rename = "tray.update")]
    TrayUpdate { items: Vec<TrayItem> },

    /// A tray item's menu, fetched and ready to draw.
    ///
    /// Sent in answer to `tray.activate` with `button` of `menu` — but only
    /// for the items that publish a `com.canonical.dbusmenu` object. An item
    /// that implements `ContextMenu` instead draws its own window and nothing
    /// comes back here, which is why this is an event rather than a reply: the
    /// shell asks the same question for every item and draws a menu only when
    /// there is one to draw.
    #[serde(rename = "tray.menu")]
    TrayMenu {
        /// Which item's menu, as `tray.update` named it.
        id: String,
        /// Where the icon was, from the request — so the menu opens under the
        /// icon that was clicked rather than wherever the shell last saw one.
        x: i32,
        y: i32,
        items: Vec<TrayMenuItem>,
    },

    /// What is playing, for the bar's media widget.
    ///
    /// Sent when it changes rather than on the status tick: a track lasts
    /// minutes and a pause is instant, so a widget driven by the two-second
    /// sample would be both stale and a redraw of the desktop every two
    /// seconds. `player` is absent when nothing is running, which is what
    /// takes the widget off the bar.
    #[serde(rename = "mpris.update")]
    MprisUpdate {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        player: Option<MprisPlayer>,
    },

    /// System statistics for the bar.
    ///
    /// `cpu` and `memory` are `-1.0` when unavailable rather than absent: the
    /// shell tests `s.cpu >= 0` (`data/shell/bar.js:113`), and an absent field
    /// would read as `undefined >= 0`, which is false but for the wrong
    /// reason. `load` is the one minute average alone, which is the only one
    /// the bar shows.
    #[serde(rename = "status.update")]
    StatusUpdate {
        cpu: f64,
        memory: f64,
        load: f64,
        net_rx: f64,
        net_tx: f64,
        disk_free: f64,
        disk_total: f64,
        /// Per-mount usage for the bar's disk widgets. Omitted when there are
        /// none, so a shell that never asked stays exactly as it was.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        mounts: Vec<MountUsage>,
        /// The default audio sink's volume in `0.0..=1.0`, or `-1.0` when the
        /// compositor could not say (no `wpctl`, no sink, no volume widget).
        #[serde(default)]
        volume: f64,
        /// Whether the default audio sink is muted. Meaningful only when a
        /// volume widget is present.
        #[serde(default)]
        muted: bool,
        /// The default audio source's (microphone) volume in `0.0..=1.0`, or
        /// `-1.0` when the compositor could not say (no `wpctl`, no source, no
        /// mic widget).
        #[serde(default)]
        mic_volume: f64,
        /// Whether the default audio source is muted. Meaningful only when a
        /// mic widget is present.
        #[serde(default)]
        mic_muted: bool,
    },

    #[serde(rename = "output.layout")]
    OutputLayout { outputs: Vec<OutputInfo> },

    /// A keybinding fired. Split on whitespace so the shell does not have to
    /// parse a free-form string (`src/ipc.c:553`).
    #[serde(rename = "shell.command")]
    ShellCommand { command: String, args: Vec<String> },

    /// An application asked to share the screen, and the user has to choose
    /// what.
    ///
    /// Re-sent, whole, every time the highlight moves. The shell draws this
    /// list and nothing else decides it: the compositor knows which windows and
    /// monitors exist and which of them the application will accept, and it
    /// routes the keys, because the shell receives no input of its own.
    #[serde(rename = "screencast.pick")]
    ScreencastPick {
        /// Which request this is, so an answer cannot land on a later one.
        id: u32,
        sources: Vec<CastSource>,
        /// Where the highlight is, as an index into `sources`.
        selected: u32,
    },

    /// The choice was made, or it was abandoned. The shell takes its chooser
    /// down either way.
    #[serde(rename = "screencast.pick.done")]
    ScreencastPickDone { id: u32 },

    /// Throw the page away and load it again, ignoring the HTTP cache.
    ///
    /// For the shell process rather than for the page, and the only event that
    /// is. The keybinding behind it used to reach an engine the compositor
    /// owned; with the shell in a process of its own there is no such call to
    /// make, so the request travels the same way everything else does and the
    /// shell process acts on it instead of forwarding it.
    ///
    /// Harmless to a shell that does forward it: `data/shell/*.js` ignores an
    /// event type it does not know.
    #[serde(rename = "shell.reload")]
    ShellReload,

    /// A rejected message. Delivered to the client that caused it where there
    /// is one, and broadcast otherwise — an error the shell caused is one it
    /// must see on the channel it already listens to.
    #[serde(rename = "error")]
    Error { context: String, message: String },
}

/// Something an application could be given a picture of.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CastSource {
    /// One of `output`, `window`, `all-outputs`, `follow-window` or
    /// `follow-output`.
    ///
    /// The first two name a particular thing and never change. The last three
    /// name whatever is in front at the time: every monitor at once, whichever
    /// window has the keyboard, whichever monitor is being worked on. A shell
    /// that does not know a kind should still draw the row — `label` and
    /// `detail` carry what it is — and the kind is there to mark it.
    pub kind: String,
    /// What to show the user: a monitor's name, a window's title, or the
    /// compositor's own name for a source that points at nothing in
    /// particular.
    pub label: String,
    /// The line under it — the make and model of a monitor, the application of
    /// a window, or what a following source will follow. Empty string, never
    /// null.
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ViewAdded {
    pub id: u32,
    pub title: String,
    pub app_id: String,
    pub output: String,

    /// So the shell can refuse to shrink a window past what its client accepts.
    /// Zero for anything that is not an xdg-toplevel.
    pub min_width: i32,
    pub min_height: i32,

    /// True when this is a replay in answer to `view.query` rather than a
    /// window that just mapped.
    pub replay: bool,

    /// Dialogs and fixed-size windows want floating rather than tiling. The
    /// compositor can see the signals — a parent toplevel, an X11 window type —
    /// and the shell cannot.
    pub floating: bool,

    /// The window this one is a dialog *of*, when it is one.
    ///
    /// The same parent link `floating` is partly inferred from, named rather
    /// than reduced to a boolean. A layout that places windows by hand needs
    /// it: a dialog belongs next to the window it came from, and putting it in
    /// the middle of the screen puts it in the middle of nothing in particular
    /// when its parent is somewhere else entirely.
    ///
    /// Absent when the window is not a dialog, and also when its parent is a
    /// surface that was never announced as a view. Omitted from the wire
    /// rather than sent as null, so a shell written against the older message
    /// sees exactly what it saw before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<u32>,

    /// The window's natural size, which is what a floating window opens at.
    pub width: i32,
    pub height: i32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// `"tiling"` when unset in the config file.
    pub layout: String,
    pub logo: bool,
    pub tutorial: bool,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bar: Option<String>,

    /// The keymap as it actually stands, so the shell can show it.
    ///
    /// The bindings live here and nowhere else — a few chords exist only in one
    /// layout, a config file may add or shadow any of them — so a shell that
    /// listed them from a table of its own would be describing a keyboard
    /// nobody has. This is what the compositor will really act on, in the order
    /// it matches them.
    ///
    /// Empty for a shell that never asked, and omitted from the wire then, so
    /// nothing changes for one written before this existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub binds: Vec<Bind>,

    /// Window rules, handed over as parsed JSON rather than a string so the
    /// shell does not parse twice inside a message it already parsed. Omitted
    /// entirely when unset, and also when the stored text fails to parse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<serde_json::Value>,

    /// The window gap settings, carried from the config file to the shell,
    /// which lands them on the `--gap` (inner) and `--gap-outer` custom
    /// properties every layout reads through `gapPx()` / `gapOuterPx()`,
    /// plus the `smart` toggle that drops the inner gap on a single window.
    /// Absent means the shell keeps its own defaults (inner 8, outer 0, smart
    /// off), so a message that omits it from an older build behaves as it
    /// always did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gaps: Option<Gaps>,

    /// The window border settings, carried from the config file to the shell,
    /// which lands them on the `--window-radius` and `--window-border` custom
    /// properties `.window` draws its corners and its edge with.
    ///
    /// The compositor reads the same number: a rounded border with a
    /// square-cornered client drawn over it is a border that is not there, so
    /// the client is cropped to the corners the shell drew — see
    /// `render::RoundedRenderElement`. Absent means the shell's own default,
    /// which is `--radius`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub border: Option<Border>,

    /// Extra bar widgets, carried from the config file to the shell, which
    /// builds an element for each and fills it from the status sample (disk,
    /// volume) or a fetch of its own (weather). Absent or empty is the default
    /// bar, untouched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bar_widgets: Option<Vec<BarWidget>>,

    /// Override of the entire right side of the bar: an explicit, ordered list
    /// of built-in modules (`"net"`, `"clock"`, ...) and widgets. When present
    /// (even empty) it replaces the default modules and `bar_widgets`; absent
    /// means the default bar.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bar_items: Option<Vec<BarItem>>,

    /// Whether directional focus may leave the monitor it is on.
    ///
    /// Both halves of the desktop need this and neither owns it. Tiling asks
    /// the compositor, which walks the windows on screen; the scrolling layout
    /// asks the shell, because the column being moved to is usually scrolled
    /// off screen and directional focus works from what is on it. So the
    /// answer travels with the rest of the config rather than living on one
    /// side, and `false` has to stop both or the same keypress crosses in one
    /// layout and not the other.
    ///
    /// Defaults to true, which is what it has always done — off the end of the
    /// strip carries on to the next monitor.
    #[serde(default = "yes")]
    pub focus_crosses_outputs: bool,

    /// How the tiling tree arranges itself: `"manual"`, `"master-stack"`,
    /// `"spiral"`, `"bsp"` or `"grid"`.
    ///
    /// The shell's business entirely — the compositor has no layout and only
    /// carries this across. Absent means `"manual"`, which is the tree of
    /// splits the shell has always built and the only one that does what the
    /// person at the keyboard said rather than what the mode says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tiling_mode: Option<String>,

    /// Whether something is being drawn *behind* the shell, so the page must
    /// stop painting a wallpaper of its own.
    ///
    /// The shell is the bottom layer of the desktop by default and its `body`
    /// gradient is what the wallpaper actually is. A terminal under it is
    /// invisible until the page gets out of the way, and only the compositor
    /// knows there is one — hence a flag rather than a theme key someone has
    /// to set twice.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub background_terminal: bool,

    /// An image to draw as the desktop background, as a URL the page can load
    /// — `file://` for the ordinary case of a picture on disk.
    ///
    /// The same reasoning as `background_terminal`, one step further: the
    /// shell's `body` *is* the wallpaper, so the only thing that can put an
    /// image there is the page itself. The compositor resolves the path,
    /// checks it exists and encodes it; what arrives here is ready to go
    /// inside a CSS `url()`.
    ///
    /// Absent is the shell's own gradient, which is what a desktop that has
    /// never been given a picture shows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wallpaper: Option<String>,

    /// How that image is fitted to the screen: `"fill"`, `"fit"`,
    /// `"stretch"`, `"center"` or `"tile"`. Absent means `"fill"`.
    ///
    /// Carried separately from the URL rather than folded into it because the
    /// two change independently — a mode is switched while trying a picture
    /// out, and swapping the picture keeps the mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wallpaper_mode: Option<String>,
}

fn yes() -> bool {
    true
}

/// Window gap settings, as `gaps` in the config file, carried to the shell.
///
/// `inner` is the space between adjacent windows, `outer` the extra space
/// around the edge of the output (added on top of `inner`), and `smart` drops
/// the inner gap for a lone window. Every field is optional so an absent one
/// leaves the shell's default standing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Gaps {
    pub inner: Option<i32>,
    pub outer: Option<i32>,
    pub smart: Option<bool>,
}

/// Window border settings, as `border` in the config file, carried to the
/// shell.
///
/// `radius` is the corner of the frame the shell draws around a window and
/// `width` is how thick that frame is, both in logical pixels. Optional so an
/// absent field leaves the shell's default standing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Border {
    pub radius: Option<i32>,
    pub width: Option<i32>,
    /// Square the corners of a window that is alone on its workspace and
    /// fills it. Absent means "whatever `gaps.smart` says", which is the pair
    /// moving together: the same lone window is the one the gaps collapse
    /// around, and rounding it after it has been pushed against the screen
    /// edge is what leaves wallpaper showing in the corners of the monitor.
    pub smart: Option<bool>,
}

/// One extra bar widget, as `bar_widgets` in the config file, carried to the
/// shell.
///
/// Tagged on `type` so it names its kind and then the options that kind takes,
/// matching `crate::config::BarWidgetConfig` from which it is copied.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum BarWidget {
    /// Free space on a particular mount; the compositor samples it with the
    /// status.
    #[serde(rename = "disk")]
    Disk {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
    /// Current conditions for a location; the shell fetches them itself.
    #[serde(rename = "weather")]
    Weather {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        location: Option<String>,
    },
    /// The default audio sink's volume, sampled by the compositor.
    #[serde(rename = "volume")]
    Volume,
    /// The default audio source's (microphone) volume and mute state, sampled
    /// by the compositor. Mirrors `volume` but reads `@DEFAULT_AUDIO_SOURCE@`.
    #[serde(rename = "mic")]
    Mic,
    /// What is playing, over MPRIS, sampled by the compositor and sent as
    /// `mpris.update` rather than with the status tick.
    #[serde(rename = "mpris")]
    Mpris,
}

/// One entry in a `bar_items` override, as `bar_items` in the config file,
/// carried to the shell.
///
/// Untagged: a bare string names a built-in module the bar already draws, and
/// an object names a widget. The shell draws the whole right side from the
/// list, in order. Mirrors `crate::config::BarItemConfig`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum BarItem {
    /// A built-in module: `mode`, `clock`, `cpu`, `memory`, `load`, `disk` or
    /// `net`.
    Module(String),
    /// An extra widget, taking the same options as a `bar_widgets` entry.
    Widget(BarWidget),
}

/// One mount's usage, as `mounts` in a `status.update` sample.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MountUsage {
    pub path: String,
    pub free: f64,
    pub total: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notification {
    pub id: u32,
    pub app_name: String,
    pub icon: String,
    pub summary: String,
    pub body: String,
    pub urgency: u8,

    /// Negative means "the server decides", zero means "never expire". Passed
    /// through as sent, because deciding is the shell's job.
    pub timeout: i32,

    pub actions: Vec<NotificationAction>,
}

/// One entry in the system tray.
///
/// Flattened from `org.kde.StatusNotifierItem`, which has thirteen properties
/// and four ways to say what an icon is. What a shell needs is a picture, a
/// label and something to send back, so that is what this carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrayItem {
    /// The bus name and object path, joined — what `tray.activate` names to
    /// say which item was clicked. Opaque to the shell: it is a key, not an
    /// address it can do anything with.
    pub id: String,

    /// What to draw under the icon, or in place of one. `Title` where the item
    /// set it, its `Id` otherwise, because an application that sets neither
    /// still has to be distinguishable from the one beside it.
    pub title: String,

    /// `"active"`, `"passive"` or `"needs-attention"`, lowercased from the
    /// item's `Status`. Passive means the application would rather not be
    /// shown; the shell decides whether that means hidden or dimmed, which is
    /// a styling question and so belongs in the stylesheet.
    pub status: String,

    /// The icon as a `data:` URL, or empty where the item offered none that
    /// could be resolved.
    ///
    /// A URL rather than a name or a path because the shell is a web page that
    /// may have been loaded over `http://`, where a `file://` image is refused
    /// — and because an icon name means nothing to a browser. The compositor
    /// resolves the name against the icon theme, or encodes the pixmap the
    /// item handed over, and what reaches here is something an `<img>` can
    /// show.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub icon: String,

    /// The tooltip's title and body, joined with a newline, or empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tooltip: String,

    /// Whether the item says its primary click should open its menu rather
    /// than activate it — `ItemIsMenu`. Passed on so the shell can style it,
    /// and acted on by the compositor, which turns that click into the menu.
    #[serde(default)]
    pub is_menu: bool,

    /// Whether the item publishes a menu this compositor can draw — a
    /// `com.canonical.dbusmenu` object it answered a layout for.
    ///
    /// The shell asks for a menu the same way either way; this only says
    /// whether asking will draw one here or hand the request to the
    /// application to draw its own window.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub has_menu: bool,
}

/// The media player the bar is showing.
///
/// Flattened from MPRIS, which every player on a Linux desktop publishes: a
/// bus name, an object, and metadata behind it. What a bar needs is what is
/// playing, whether it is playing, and which buttons are worth drawing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MprisPlayer {
    /// The player's bus name without the MPRIS prefix — `spotify`, `mpv`,
    /// `firefox.instance_1_15`. What it calls itself, and what the shell shows
    /// where a track has no title.
    pub id: String,

    pub title: String,
    /// Every artist the track names, joined — a track can have several and
    /// players send a list even for the one.
    pub artist: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub album: String,

    /// `"playing"`, `"paused"` or `"stopped"`, lowercased from MPRIS.
    pub status: String,

    /// Cover art the page can draw: a `data:` URL for a local file, the
    /// player's own `https://` URL where it gave one, or empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub art: String,

    /// Which buttons are worth drawing. A player answers all four and they are
    /// not decoration: `can_pause` is false for a live stream that can only be
    /// stopped, and a button that does nothing is worse than no button.
    #[serde(default)]
    pub can_go_next: bool,
    #[serde(default)]
    pub can_go_previous: bool,
    #[serde(default)]
    pub can_pause: bool,
    #[serde(default)]
    pub can_play: bool,
}

/// One row of a tray item's menu.
///
/// Flattened from `com.canonical.dbusmenu`, whose properties are an open map
/// of anything an application cares to put in it. What a shell needs to draw a
/// menu is a label, whether the row can be chosen, whether it is ticked, and
/// what is under it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrayMenuItem {
    /// The item's own id, as `tray.menu.click` names it. Assigned by the
    /// application, unique within its menu, and meaningless anywhere else.
    pub id: i32,

    pub label: String,

    /// `"standard"` or `"separator"`. A separator has no label and cannot be
    /// chosen; it is a line.
    pub kind: String,

    /// False for a row that is shown greyed out rather than hidden — an
    /// application says which, and a shell that drew a disabled row as a live
    /// one would send clicks nothing acts on.
    pub enabled: bool,

    /// `""`, `"checkmark"` or `"radio"`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub toggle: String,

    /// Whether that tick or dot is on. Meaningless without `toggle`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub checked: bool,

    /// The row's icon as a `data:` URL, resolved the same way an item's own
    /// icon is, or empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub icon: String,

    /// A submenu, already fetched. The whole tree arrives at once rather than
    /// a level at a time: a menu is small, the shell draws it in one pass, and
    /// a second round trip per submenu would be a menu that opens in stages
    /// over a compositor that is trying to hold a frame.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<TrayMenuItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationAction {
    /// What comes back on `notification.action`.
    pub key: String,
    /// What the shell draws on the button.
    pub label: String,
}

/// One entry of the keymap, as the shell shows it.
///
/// Both fields are spelled the way a config file spells them, so a listing can
/// be read straight back into `binds` — someone looking at a chord they want to
/// change should not have to translate it first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bind {
    /// `"Mod4+Shift+q"`.
    pub chord: String,
    /// `"close"`, `"exec foot"`, `"shell canvas.pan left"`.
    pub action: String,
    /// The binding mode this belongs to, empty for the ordinary keymap.
    /// `"resize"` for the chords that only fire inside resize mode.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub mode: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputInfo {
    pub name: String,
    /// Empty string, never null, when the display did not say.
    pub make: String,
    pub model: String,
    pub serial: String,

    pub enabled: bool,

    /// The output the shell last called active: where a new window opens, and
    /// what "the screen" means to anything outside the shell — a screenshot
    /// tool has no other way to ask, because `output.active` only ever went
    /// the other way.
    ///
    /// Worked out when the layout is built, so a `output.query` answers with
    /// the current one. A client that only listens to broadcasts sees the
    /// value from whenever the layout last changed.
    ///
    /// Defaulted: a session from before this field existed still parses, and
    /// says no output is active rather than refusing the whole event.
    #[serde(default)]
    pub active: bool,

    /// Position and size in the output layout.
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,

    /// The area panels have not reserved. The shell places windows inside this
    /// rather than the full box, which is what keeps a bar from overlapping
    /// them.
    pub usable_x: i32,
    pub usable_y: i32,
    pub usable_width: i32,
    pub usable_height: i32,

    /// Whether this output is in HDR now, and whether it could be — so the
    /// shell can offer the switch only where the display will take it.
    pub hdr: bool,
    pub hdr_capable: bool,

    pub scale: f64,
    pub transform: Transform,

    pub modes: Vec<Mode>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mode {
    pub width: i32,
    pub height: i32,
    /// In mHz, as the kernel reports it.
    pub refresh: i32,
    pub preferred: bool,
    pub current: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(event: &Event) -> serde_json::Value {
        serde_json::to_value(event).expect("should serialise")
    }

    #[test]
    fn tag_is_a_type_member_not_a_wrapper() {
        let value = json(&Event::ViewRemoved { id: 42 });
        assert_eq!(value["type"], "view.removed");
        assert_eq!(value["id"], 42);
        assert_eq!(value.as_object().unwrap().len(), 2);
    }

    #[test]
    fn view_added_carries_every_field_the_shell_reads() {
        let value = json(&Event::ViewAdded(ViewAdded {
            id: 1,
            title: "term".into(),
            app_id: "foot".into(),
            output: "DP-1".into(),
            min_width: 0,
            min_height: 0,
            replay: false,
            floating: false,
            parent: None,
            width: 800,
            height: 600,
        }));
        for key in [
            "type",
            "id",
            "title",
            "app_id",
            "output",
            "min_width",
            "min_height",
            "replay",
            "floating",
            "width",
            "height",
        ] {
            assert!(value.get(key).is_some(), "missing {key}");
        }
        // Not this one: an ordinary window is nobody's dialog, and the field is
        // left out entirely rather than sent as null, so a shell written
        // against the older message sees exactly what it always saw.
        assert!(value.get("parent").is_none());
    }

    /// And it is there when there is one, by the id the shell knows the parent
    /// by — a dialog is placed *on* its parent by a layout that places windows
    /// by hand, and cannot be without knowing which window that is.
    #[test]
    fn a_dialog_carries_the_window_it_belongs_to() {
        let value = json(&Event::ViewAdded(ViewAdded {
            id: 2,
            title: "save as".into(),
            app_id: "chrome".into(),
            output: "DP-1".into(),
            min_width: 0,
            min_height: 0,
            replay: false,
            floating: true,
            parent: Some(1),
            width: 300,
            height: 200,
        }));
        assert_eq!(value["parent"], 1);
    }

    #[test]
    fn unset_config_members_are_omitted_not_null() {
        let value = json(&Event::Config(Config {
            layout: "tiling".into(),
            logo: true,
            tutorial: false,
            binds: Vec::new(),
            bar: None,
            rules: None,
            theme: None,
            gaps: None,
            border: None,
            bar_widgets: None,
            bar_items: None,
            focus_crosses_outputs: true,
            tiling_mode: None,
            background_terminal: false,
            wallpaper: None,
            wallpaper_mode: None,
        }));
        assert!(value.get("bar").is_none());
        assert!(value.get("rules").is_none());
        assert!(value.get("theme").is_none());
        assert!(value.get("gaps").is_none());
        assert!(value.get("border").is_none());
        assert_eq!(value["layout"], "tiling");
    }

    #[test]
    fn the_border_is_carried_to_the_shell() {
        let value = json(&Event::Config(Config {
            layout: "tiling".into(),
            logo: true,
            tutorial: true,
            binds: Vec::new(),
            bar: None,
            rules: None,
            theme: None,
            gaps: None,
            border: Some(Border {
                radius: Some(12),
                width: Some(3),
                smart: Some(true),
            }),
            bar_widgets: None,
            bar_items: None,
            focus_crosses_outputs: true,
            tiling_mode: None,
            background_terminal: false,
            wallpaper: None,
            wallpaper_mode: None,
        }));
        assert_eq!(value["border"]["radius"], 12);
        assert_eq!(value["border"]["width"], 3);
        assert_eq!(value["border"]["smart"], true);
    }

    #[test]
    fn gaps_are_carried_to_the_shell_with_all_three_fields() {
        let value = json(&Event::Config(Config {
            layout: "tiling".into(),
            logo: true,
            tutorial: true,
            binds: Vec::new(),
            bar: None,
            rules: None,
            theme: None,
            gaps: Some(Gaps {
                inner: Some(15),
                outer: Some(4),
                smart: Some(true),
            }),
            border: None,
            bar_widgets: None,
            bar_items: None,
            focus_crosses_outputs: true,
            tiling_mode: None,
            background_terminal: false,
            wallpaper: None,
            wallpaper_mode: None,
        }));
        assert_eq!(value["gaps"]["inner"], 15);
        assert_eq!(value["gaps"]["outer"], 4);
        assert_eq!(value["gaps"]["smart"], true);
    }

    #[test]
    fn a_config_without_the_crossing_key_reads_as_crossing() {
        // The shell and the compositor have to agree on the default, and a
        // message from a build that predates the key has to keep the old
        // behaviour rather than silently trapping focus on one monitor.
        let config: Config = serde_json::from_value(serde_json::json!({
            "layout": "scrolling",
            "logo": true,
            "tutorial": true,
        }))
        .expect("a config without the key");
        assert!(config.focus_crosses_outputs);
    }

    #[test]
    fn crossing_is_carried_to_the_shell_when_it_is_off() {
        // The scrolling layout decides this in the shell, so `false` has to
        // survive the trip — the compositor honouring it alone would stop
        // tiling from crossing and leave the strip crossing anyway.
        let value = json(&Event::Config(Config {
            layout: "scrolling".into(),
            logo: true,
            tutorial: true,
            binds: Vec::new(),
            bar: None,
            rules: None,
            theme: None,
            gaps: None,
            border: None,
            bar_widgets: None,
            bar_items: None,
            focus_crosses_outputs: false,
            tiling_mode: None,
            background_terminal: false,
            wallpaper: None,
            wallpaper_mode: None,
        }));
        assert_eq!(value["focus_crosses_outputs"], false);
    }

    #[test]
    fn rules_go_over_as_parsed_json() {
        let value = json(&Event::Config(Config {
            layout: "scrolling".into(),
            logo: false,
            tutorial: false,
            binds: Vec::new(),
            bar: Some("top".into()),
            rules: Some(serde_json::json!([{"app_id": "mpv", "floating": true}])),
            theme: None,
            gaps: None,
            border: None,
            bar_widgets: None,
            bar_items: None,
            focus_crosses_outputs: true,
            tiling_mode: None,
            background_terminal: false,
            wallpaper: None,
            wallpaper_mode: None,
        }));
        assert!(value["rules"].is_array(), "rules must not be a string");
        assert_eq!(value["rules"][0]["app_id"], "mpv");
    }

    #[test]
    fn shell_command_splits_command_from_args() {
        let value = json(&Event::ShellCommand {
            command: "workspace.switch".into(),
            args: vec!["3".into()],
        });
        assert_eq!(value["command"], "workspace.switch");
        assert_eq!(value["args"][0], "3");
    }

    #[test]
    fn output_layout_round_trips() {
        let event = Event::OutputLayout {
            outputs: vec![OutputInfo {
                name: "DP-1".into(),
                make: String::new(),
                model: String::new(),
                serial: String::new(),
                enabled: true,
                active: true,
                x: 0,
                y: 0,
                width: 2560,
                height: 1440,
                usable_x: 0,
                usable_y: 32,
                usable_width: 2560,
                usable_height: 1408,
                hdr: false,
                hdr_capable: true,
                scale: 1.0,
                transform: Transform::Normal,
                modes: vec![Mode {
                    width: 2560,
                    height: 1440,
                    refresh: 143998,
                    preferred: true,
                    current: true,
                }],
            }],
        };
        let text = serde_json::to_string(&event).unwrap();
        assert_eq!(serde_json::from_str::<Event>(&text).unwrap(), event);
        // The empty-string convention, not null.
        assert!(text.contains(r#""make":"""#));
    }

    #[test]
    fn every_notify_function_has_a_variant() {
        let events = [
            json(&Event::ViewAdded(ViewAdded {
                id: 1,
                title: String::new(),
                app_id: String::new(),
                output: String::new(),
                min_width: 0,
                min_height: 0,
                replay: true,
                floating: false,
                parent: None,
                width: 0,
                height: 0,
            })),
            json(&Event::ViewRemoved { id: 1 }),
            json(&Event::ViewProps {
                id: 1,
                title: String::new(),
                app_id: String::new(),
                icon: None,
            }),
            json(&Event::ViewFocused { id: 1 }),
            json(&Event::Config(Config {
                layout: "tiling".into(),
                logo: false,
                tutorial: false,
                binds: Vec::new(),
                bar: None,
                rules: None,
                theme: None,
                gaps: None,
                border: None,
                bar_widgets: None,
                bar_items: None,
                focus_crosses_outputs: true,
                tiling_mode: None,
                background_terminal: false,
                wallpaper: None,
                wallpaper_mode: None,
            })),
            json(&Event::Modifiers { logo: true }),
            json(&Event::SessionRestore {
                state: String::new(),
            }),
            json(&Event::NotificationAdd(Notification {
                id: 1,
                app_name: String::new(),
                icon: String::new(),
                summary: String::new(),
                body: String::new(),
                urgency: 1,
                timeout: -1,
                actions: Vec::new(),
            })),
            json(&Event::NotificationClose { id: 1 }),
            json(&Event::MprisUpdate { player: None }),
            json(&Event::TrayUpdate { items: Vec::new() }),
            json(&Event::TrayMenu {
                id: String::new(),
                x: 0,
                y: 0,
                items: Vec::new(),
            }),
            json(&Event::OutputLayout {
                outputs: Vec::new(),
            }),
            json(&Event::ScreencastPick {
                id: 0,
                sources: Vec::new(),
                selected: 0,
            }),
            json(&Event::ScreencastPickDone { id: 0 }),
            json(&Event::ShellCommand {
                command: String::new(),
                args: Vec::new(),
            }),
            json(&Event::Error {
                context: String::new(),
                message: String::new(),
            }),
        ];

        let names: Vec<&str> = events.iter().map(|e| e["type"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            [
                "view.added",
                "view.removed",
                "view.props",
                "view.focused",
                "config",
                "modifiers",
                "session.restore",
                "notification.add",
                "notification.close",
                "mpris.update",
                "tray.update",
                "tray.menu",
                "output.layout",
                "screencast.pick",
                "screencast.pick.done",
                "shell.command",
                "error",
            ]
        );
    }
}
