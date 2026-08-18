// SPDX-License-Identifier: MIT
//
// Inbound messages: shell to compositor. Ports the dispatch table at
// src/ipc.c:1415 and the handlers above it.
//
// Field-level fidelity matters more than it looks. Several handlers distinguish
// "absent" from "present and false" — `view.visible` defaults to true when the
// key is missing, `output.hdr` *toggles* when `enabled` is absent rather than
// disabling — so the defaults encoded here are part of the protocol, not
// convenience.

use serde::{Deserialize, Serialize};

use crate::geometry::{Box, PartialBox, Transform};

/// A message from the shell to the compositor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Request {
    /// Place a window. The shell measured a hole in its own DOM and is telling
    /// the compositor where it landed.
    #[serde(rename = "view.layout")]
    ViewLayout(ViewLayout),

    #[serde(rename = "view.visible")]
    ViewVisible {
        #[serde(deserialize_with = "view_id")]
        id: u32,
        /// Absent means visible (`src/ipc.c:919`).
        #[serde(default = "yes")]
        visible: bool,
    },

    #[serde(rename = "view.fullscreen")]
    ViewFullscreen {
        #[serde(deserialize_with = "view_id")]
        id: u32,
        /// Absent means not fullscreen (`src/ipc.c:1182`).
        #[serde(default)]
        fullscreen: bool,
    },

    #[serde(rename = "view.focus")]
    ViewFocus {
        #[serde(deserialize_with = "view_id")]
        id: u32,
    },

    #[serde(rename = "view.close")]
    ViewClose {
        #[serde(deserialize_with = "view_id")]
        id: u32,
    },

    /// Per-window opacity, driven a frame at a time by a tween in the shell.
    /// The shell cannot fade a window with CSS: the frame is DOM, the contents
    /// are a surface the compositor draws.
    #[serde(rename = "view.opacity")]
    ViewOpacity {
        #[serde(deserialize_with = "view_id")]
        id: u32,
        /// Absent means opaque; the value is clamped to `0.0..=1.0`
        /// (`src/ipc.c:897`).
        #[serde(default = "one")]
        opacity: f64,
    },

    /// Ask for `config` and one `view.added` per mapped window.
    #[serde(rename = "view.query")]
    ViewQuery,

    /// Give the keyboard to the wallpaper terminal on the active monitor, or
    /// take it back.
    ///
    /// The same toggle the `background` keybinding fires, and the only other
    /// way in. It is a request rather than a property of the client because
    /// the whole point of that client is that focus never reaches it by
    /// accident: it is not a view, so `view.focus` cannot name it, and this
    /// says "deliberately, now".
    #[serde(rename = "background.focus")]
    BackgroundFocus,

    /// Move keyboard focus to the shell itself.
    #[serde(rename = "shell.focus")]
    ShellFocus,

    /// Enter or leave the overview. While it is up the shell draws miniatures
    /// of every window and input is routed to the shell.
    /// Where the shell drew something that belongs above the windows, in the
    /// layout's own coordinates.
    ///
    /// The shell is one buffer under the whole desktop — the windows are
    /// painted into holes in it — so anything it draws is behind them unless
    /// it says where. A zero size means there is nothing on top any more.
    #[serde(rename = "screencast.rect")]
    ScreencastRect {
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    },

    #[serde(rename = "shell.overview")]
    ShellOverview {
        #[serde(default)]
        active: bool,
    },

    /// Store the shell's own serialisation verbatim.
    #[serde(rename = "session.save")]
    SessionSave { state: String },

    #[serde(rename = "session.query")]
    SessionQuery,

    #[serde(rename = "notification.action")]
    NotificationAction {
        id: u32,
        /// The key the application supplied, not the label the shell drew.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        action: Option<String>,
    },

    #[serde(rename = "notification.dismiss")]
    NotificationDismiss { id: u32 },

    #[serde(rename = "notification.expire")]
    NotificationExpire { id: u32 },

    /// A tray item was clicked, or scrolled over.
    ///
    /// The shell draws the tray, so the shell is what knows which item was
    /// hit and where on the screen it sits — an application opening its own
    /// menu is told the pointer's position and puts its window there, so the
    /// coordinates are part of the message rather than something the
    /// compositor guesses afterwards.
    #[serde(rename = "tray.activate")]
    TrayActivate {
        /// The `id` from `tray.update`. An item that has since gone is
        /// ignored rather than refused: a click landing as an application
        /// exits is a race, not a mistake the user can do anything about.
        id: String,
        /// `"primary"`, `"secondary"` or `"menu"`. Anything else is refused.
        /// An item that says `is_menu` gets its menu from a primary click too,
        /// which is what the property is for.
        #[serde(default)]
        button: String,
        /// Where the pointer was, in the layout's own coordinates — what the
        /// item is handed so it can place its own window.
        #[serde(default)]
        x: i32,
        #[serde(default)]
        y: i32,
    },

    /// A row of an open tray menu was chosen.
    ///
    /// The menu is the compositor's: it fetched the layout and the shell drew
    /// what it was handed, so the row is named by the id the application gave
    /// it rather than by anything the shell made up.
    #[serde(rename = "tray.menu.click")]
    TrayMenuClick {
        /// Which item's menu, as `tray.update` named it.
        id: String,
        /// The row, as `tray.menu` named it.
        item: i32,
    },

    /// An open tray menu was dismissed without a choice.
    ///
    /// Applications are told, because a menu that is never closed is one they
    /// believe is still on screen — several rebuild their menu on close, and
    /// one that is never told keeps serving a stale one.
    #[serde(rename = "tray.menu.closed")]
    TrayMenuClosed { id: String },

    /// The wheel turned over a tray item.
    #[serde(rename = "tray.scroll")]
    TrayScroll {
        id: String,
        /// Wheel steps: positive is up, or right for a horizontal wheel.
        delta: i32,
        /// `"vertical"` or `"horizontal"`.
        #[serde(default)]
        orientation: String,
    },

    /// Where the shell has drawn something that belongs above the windows.
    ///
    /// The shell is one buffer *under* the clients, so anything it draws over
    /// one is behind it by construction — a notification, the screen-share
    /// chooser, anything else that floats. Naming the rectangles lets the
    /// compositor draw the same buffer again, cropped to each, in front.
    ///
    /// Sent whole: the list replaces whatever was there, and an empty list
    /// means nothing floats now. `screencast.rect` is the older single-rectangle
    /// form of this and still works.
    #[serde(rename = "shell.overlay")]
    ShellOverlay {
        #[serde(default)]
        rects: Vec<OverlayRect>,
    },

    /// The shell's workspaces, whole, whenever they change.
    ///
    /// Workspaces are the shell's: it decides how many there are, what they
    /// are called and which is on which screen, and the compositor has never
    /// needed to know. `ext-workspace-v1` is a client asking to be told, so
    /// the shell says, and the compositor relays. Sent whole rather than as a
    /// diff because the shell already has the list and reconciling two
    /// halves of one is how they drift apart.
    #[serde(rename = "workspace.list")]
    WorkspaceList {
        #[serde(default)]
        workspaces: Vec<Workspace>,
    },

    #[serde(rename = "output.configure")]
    OutputConfigure(OutputConfigure),

    #[serde(rename = "output.hdr")]
    OutputHdr {
        /// Absent means the active output.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// Absent *toggles* — what a keybinding wants, and what a settings
        /// panel does not (`src/ipc.c:1321`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        enabled: Option<bool>,
    },

    /// The user accepted an output change, so cancel the pending revert.
    #[serde(rename = "output.confirm")]
    OutputConfirm,

    #[serde(rename = "output.active")]
    OutputActive { name: String },

    #[serde(rename = "output.query")]
    OutputQuery,

    /// Headless-only: plug a virtual monitor. Rejected on a real session.
    #[serde(rename = "output.test_add")]
    OutputTestAdd,

    /// Headless-only: unplug one. Absent name means the first output.
    #[serde(rename = "output.test_remove")]
    OutputTestRemove {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },

    /// A runtime keybinding. Additive and expendable — binds that must survive
    /// a broken shell belong in the config file.
    #[serde(rename = "bind.add")]
    BindAdd { chord: String, action: String },

    /// Ask the shell to do something, as a keybinding would.
    ///
    /// Every other request here is the shell talking to the compositor. This
    /// one goes the other way and is the only one that does: it is re-emitted
    /// as the `shell.command` event a bound chord already produces, so the
    /// shell cannot tell the two apart and needs no code for it.
    ///
    /// It exists because the layout is entirely the shell's and, until now,
    /// keyboard input was the only thing that could reach it. Anything wanting
    /// to switch a workspace, focus the next monitor or change the layout
    /// model had to be a person pressing a key — which leaves a benchmark
    /// unable to put a window on a chosen screen, and a test unable to drive
    /// any of it. `bind.add` is not an answer: it binds a chord, and nothing
    /// here can press one.
    ///
    /// Deliberately the whole verb set and not a chosen few. The shell already
    /// ignores commands it does not recognise — `handleShellCommand` warns and
    /// returns — so the compositor validating a list here would be a second
    /// place to keep in step with `data/shell/commands.js`, and it has no way
    /// to know what a given shell understands.
    #[serde(rename = "shell.command")]
    ShellCommand {
        command: String,
        #[serde(default)]
        args: Vec<String>,
    },

    /// Run a shell command on the host, out from under the compositor.
    ///
    /// The bridge the bar's widgets use to *do* something: open a directory in
    /// the default file manager, open a place in a browser, or drive the audio
    /// sink through `wpctl`. The shell page cannot spawn a process itself — it
    /// is a sandboxed web view — so it asks the compositor, which runs the line
    /// through the same `/bin/sh` path a keybinding's `exec` uses. The caller
    /// composes the exact command line; the compositor just runs it.
    ///
    /// Not a privilege escalation: this socket already runs keybindings through
    /// `shell.command`, which can execute anything, and it is 0600.
    #[serde(rename = "shell.exec")]
    ShellExec { command: String },

    /// Ask the compositor to sample the status bar's numbers again now,
    /// rather than waiting for the next two-second tick.
    ///
    /// An audio widget uses this after it has driven the sink through `wpctl`
    /// — scrolled the volume or toggled mute — so the change appears on screen
    /// immediately. Without it the display lags up to two seconds, which makes
    /// a mute that worked look like one that did not.
    #[serde(rename = "status.refresh")]
    StatusRefresh,

    /// Change the volume of the default sink or source, and re-sample the bar
    /// once it has actually changed.
    ///
    /// The shell used to do this itself: `shell.exec wpctl set-volume …`
    /// followed by `status.refresh`. Both are true and the pair is a race —
    /// `shell.exec` spawns and returns, so the refresh samples the sink before
    /// the child that changes it has run. What that looks like is a volume
    /// that scrolls and a bar that shows the old number for up to two seconds,
    /// which reads as the scroll having missed.
    ///
    /// Here the compositor runs the change to completion and samples after, so
    /// the ordering is not a matter of timing. It is also the compositor's own
    /// business: the same `wpctl` it already samples with, for a widget it
    /// draws itself — the shell needs no way to run programs for this.
    #[serde(rename = "status.volume")]
    StatusVolume {
        /// Which node: `"sink"` for the speakers, `"source"` for the
        /// microphone. Anything else is refused.
        target: String,
        /// Percentage points to add, negative to subtract. Absent, or zero,
        /// changes no volume — which is what a mute-only message is.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        delta: Option<i32>,
        /// Toggle the mute state of that node. Applied after `delta`, so one
        /// message can do both.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        mute: bool,
    },

    /// Move the pointer, in the layout's own coordinates.
    ///
    /// For driving the desktop from a script: a test that wants to know
    /// whether a notification can be clicked has to be able to click it, and
    /// there is no other way in. Everything a real pointer does goes through
    /// the same path — the hit test, the focus, the shell's overlays — so what
    /// this exercises is what a hand exercises.
    ///
    /// Not a privilege escalation: this socket already runs keybindings
    /// through `shell.command`, which can execute anything, and it is 0600.
    #[serde(rename = "input.pointer")]
    InputPointer { x: f64, y: f64 },

    /// Press or release a pointer button, at wherever the pointer is.
    ///
    /// `button` is an evdev code — 272 is left, 273 right, 274 middle — which
    /// is what the compositor receives from libinput and what the shell is
    /// written against.
    #[serde(rename = "input.button")]
    InputButton {
        button: u32,
        #[serde(default = "crate::request::pressed_default")]
        pressed: bool,
    },

    /// Press or release a key, by evdev keycode.
    ///
    /// The same codes libinput reports — 29 is left control, 56 left alt, 34
    /// is `g` — because that is what the compositor's own key path takes, and a
    /// scripted chord has to go through the filter that decides what a chord
    /// means.
    #[serde(rename = "input.key")]
    InputKey {
        keycode: u32,
        #[serde(default = "crate::request::pressed_default")]
        pressed: bool,
    },

    #[serde(rename = "quit")]
    Quit,

    /// Set the window gaps at runtime, as the `gaps` block in the config
    /// file.
    ///
    /// The shell reads the gaps from the `--gap` (inner) and `--gap-outer`
    /// CSS custom properties, so this updates the compositor's `Config` and
    /// re-sends it, exactly as a config reload would — without touching the
    /// file on disk. Useful for a keybinding, a settings panel, or a script
    /// that wants to try a value before editing the config file.
    ///
    /// Every field is optional; only the ones given are changed. A gap of
    /// zero is deliberate (no spacing at all) and accepted; a negative gap is
    /// rejected.
    #[serde(rename = "config.gaps")]
    ConfigGaps {
        /// Pixels between adjacent windows, as `gaps.inner`. Negative values
        /// are rejected.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        inner: Option<i32>,
        /// Pixels of extra space around the output edge, as `gaps.outer`.
        /// Negative values are rejected.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        outer: Option<i32>,
        /// Drop the inner gap for a lone window, as `gaps.smart`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        smart: Option<bool>,
    },

    /// Set the window border at runtime, as the `border` block in the config
    /// file.
    ///
    /// The same path as `config.gaps`: the compositor's `Config` is updated
    /// and re-sent, and the shell lands the radius on the `--window-radius`
    /// custom property. Unlike the gaps this is not the shell's business
    /// alone — the compositor crops each client to the corners the shell
    /// draws — so the number is read on both sides of the same message.
    ///
    /// Every field is optional; only the ones given are changed. A radius of
    /// zero is deliberate (square corners); a negative one is rejected.
    #[serde(rename = "config.border")]
    ConfigBorder {
        /// The corner radius in logical pixels, as `border.radius`. Negative
        /// values are rejected.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        radius: Option<i32>,
        /// How thick the border is, in logical pixels, as `border.width`.
        /// Zero is no border; negative values are rejected.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        width: Option<i32>,
        /// Square the corners of a lone window, as `border.smart`. Absent
        /// here leaves whatever is set; absent from the *config* means the
        /// setting follows `gaps.smart`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        smart: Option<bool>,
    },

    /// Set the wallpaper at runtime, as the `wallpaper` and `wallpaper_mode`
    /// keys in the config file.
    ///
    /// The same path as `config.gaps`: the compositor resolves what it is
    /// given, updates its `Config` and re-sends it, and the shell paints the
    /// picture as the desktop background. Nothing is written to disk, so this
    /// is what a wallpaper cycler, a settings panel or a keybinding uses — a
    /// new picture every hour costs one message and no config reload.
    ///
    /// Both fields are optional and only the ones given change, so the mode
    /// can be switched without naming the picture again and the other way
    /// round.
    #[serde(rename = "config.wallpaper")]
    ConfigWallpaper {
        /// A path or a URL for the image, as `wallpaper` in the config file.
        /// A local path is checked and rejected if it is not there — a
        /// wallpaper that silently does not appear is the failure this exists
        /// to avoid.
        ///
        /// The empty string takes the wallpaper away and leaves the shell's
        /// own background, which is the only way back to it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        /// How it is fitted: `fill`, `fit`, `stretch`, `center` or `tile`, as
        /// `wallpaper_mode`. An unknown mode is rejected rather than
        /// defaulting.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mode: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ViewLayout {
    #[serde(deserialize_with = "view_id")]
    pub id: u32,

    /// Absent fields keep the view's current geometry (`src/ipc.c:833`).
    #[serde(flatten)]
    pub box_: PartialBox,

    /// Draw the window shrunk without resizing its client. The overview needs
    /// every window on screen at once, and plenty of clients have a minimum
    /// size larger than a thumbnail. Clamped to `0.0..=1.0`, with anything
    /// outside that treated as 1.0 (`src/ipc.c:853`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<f64>,

    /// The part of the window actually on its output. Absent means all of it,
    /// which is the ordinary tiled case. Absent *fields* fall back to the
    /// resolved layout box, not to the view's current clip (`src/ipc.c:862`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clip: Option<PartialBox>,

    /// The frame the shell drew around this window — border and all — where
    /// that frame has to be drawn above the windows beneath it.
    ///
    /// The shell is one buffer under the whole desktop, so everything it paints
    /// is behind every client surface. A tiled border is never noticed, because
    /// it falls in the gap between two windows where there is no surface to
    /// hide it. A floating window sits *over* another window, and its border
    /// lands inside that window's hole — where the client's own surface covers
    /// it, which is a floating window drawn with no border at all.
    ///
    /// Naming the rectangle lets the compositor draw that piece of the shell
    /// again, above the windows this one is stacked over. Absent for a window
    /// that needs nothing of the sort, which is most of them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<crate::geometry::Box>,

    /// Whether the shell is floating this window rather than tiling it.
    ///
    /// The shell owns layout, but not stacking: the stack lives in the
    /// compositor's `Space`, which is what the renderer draws from and what a
    /// click is tested against. A floating window that fell behind a tiled one
    /// is invisible to both, so the compositor has to know which windows are
    /// floating to keep them above the rest — and the shell is the only thing
    /// that knows.
    ///
    /// Absent means tiled, which is most windows and every window a shell that
    /// never sets it sends.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub floating: bool,

    /// Whether the frame the shell drew around this window has square corners,
    /// whatever `border.radius` says.
    ///
    /// The compositor crops each client to the corner the page drew, and only
    /// the shell knows when it did not draw one: smart radius squares the lone
    /// window on a workspace, because a rounded window against the edge of the
    /// screen is four notches of wallpaper in the corners of the monitor. That
    /// is a question about the layout, and the layout is the shell's.
    ///
    /// Absent means the configured corner, which is every other window.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub square: bool,
}

impl ViewLayout {
    /// Resolve the wire message against the view's current geometry.
    ///
    /// Returns `None` for a degenerate box, which the C build drops without
    /// reporting an error (`src/ipc.c:839`).
    pub fn resolve(&self, current: Box) -> Option<Resolved> {
        let box_ = self.box_.resolve(current);
        if !box_.is_valid() {
            return None;
        }

        // Out-of-range is not an error, it is 1.0. A shell mid-animation can
        // overshoot and should not have the frame rejected for it.
        let scale = match self.scale {
            Some(s) if s > 0.0 && s <= 1.0 => s,
            _ => 1.0,
        };

        Some(Resolved {
            box_,
            scale,
            // Note the fallback: an absent clip field resolves against the new
            // layout box, so a clip of `{}` means "the whole window".
            clip: self.clip.map(|c| c.resolve(box_)),
        })
    }
}

/// A [`ViewLayout`] with every default filled in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Resolved {
    pub box_: Box,
    pub scale: f64,
    pub clip: Option<Box>,
}

/// A rectangle of the shell that belongs above the windows.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct OverlayRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    /// Drawn above the windows, but not in the way of the pointer.
    ///
    /// An overlay normally takes the click: a notification over a window is
    /// visible, so a click on it is a click on the notification and not on
    /// whatever it covers. The bar under `auto` is the exception. It is only
    /// on screen while Mod4 is held, and Mod4 is also the modifier every
    /// window gesture is on — so a bar that took the pointer took every
    /// Mod4+click and Mod4+drag in the strip it floats over, and a window
    /// dragged up there could no longer be focused, moved or resized. The bar
    /// loses nothing by declining them: a click there is a Mod4+click, which
    /// the compositor keeps for the gesture and never forwards to the shell.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub passthrough: bool,
}

/// One of the shell's workspaces, as an outside client sees it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Workspace {
    /// The shell's own identifier, stable for the life of the workspace. It
    /// goes out on the wire as `ext_workspace_handle_v1.id`, which is what a
    /// bar uses to tell one workspace from another across a restart.
    pub id: String,
    /// What to show. Absent is allowed by the protocol; a name nobody set is
    /// better empty than invented.
    #[serde(default)]
    pub name: String,
    /// Which screen it belongs to, by output name. Absent means it belongs to
    /// no screen in particular, which the protocol expresses as a workspace in
    /// no group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub urgent: bool,
    /// The protocol's third state: exists, is not shown, is not urgent.
    #[serde(default)]
    pub hidden: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputConfigure {
    pub name: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<ModeRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transform: Option<Transform>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_sync: Option<bool>,

    /// Position in the output layout. Applied after the mode commit succeeds,
    /// and each axis independently — sending only `x` keeps the current `y`
    /// (`src/ipc.c:1150`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y: Option<i32>,
}

/// A requested mode. The compositor prefers an exact modeline the display
/// advertised and falls back to a custom mode, so unusual panels stay
/// configurable (`src/ipc.c:1095`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModeRequest {
    #[serde(default)]
    pub width: i32,
    #[serde(default)]
    pub height: i32,
    /// Zero means "any refresh at this size".
    #[serde(default)]
    pub refresh: i32,
}

/// A view id the way C reads one: `(uint32_t)object_int(object, "id", 0)`.
///
/// The shell sends -1 for "no view" — `session.js` passes on whatever
/// `firstOf` returned for an empty column. C cast that to `0xffffffff`, found
/// no window with it, and did nothing. Refusing the message instead rejects
/// the whole request, so an unfocus turns into a parse error and the shell
/// logs a console error for a message the C build accepted every day.
fn view_id<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    // Widest signed type first so both -1 and 0xffffffff land here, then the
    // same truncating cast C performs.
    let raw = i64::deserialize(deserializer)?;
    Ok(raw as u32)
}

fn yes() -> bool {
    true
}

fn one() -> f64 {
    1.0
}

/// A button message with no `pressed` is a press: the common case is a click,
/// and a release with nothing held is a no-op anyway.
fn pressed_default() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Request {
        serde_json::from_str(json).expect("should parse")
    }

    #[test]
    fn a_negative_view_id_parses_the_way_c_cast_it() {
        // The shell sends -1 for "no view". C cast it to 0xffffffff, matched
        // no window and did nothing; rejecting the message instead turns an
        // unfocus into a parse error the shell reports on its console.
        assert_eq!(
            parse(r#"{"type":"view.focus","id":-1}"#),
            Request::ViewFocus { id: u32::MAX }
        );
        // Every id field C read through the same cast.
        assert_eq!(
            parse(r#"{"type":"view.close","id":-1}"#),
            Request::ViewClose { id: u32::MAX }
        );
        assert!(matches!(
            parse(r#"{"type":"view.layout","id":-1,"x":0,"y":0,"width":1,"height":1}"#),
            Request::ViewLayout(layout) if layout.id == u32::MAX
        ));
    }

    #[test]
    fn absent_visible_means_visible() {
        assert_eq!(
            parse(r#"{"type":"view.visible","id":7}"#),
            Request::ViewVisible {
                id: 7,
                visible: true
            }
        );
    }

    #[test]
    fn an_overlay_takes_the_pointer_unless_it_says_otherwise() {
        // Taking it is the default, and has to be: a notification drawn over a
        // window is visible, so a click on it belongs to the notification. The
        // shell asks for the other behaviour by name, and only for the bar
        // under 'auto' — see `passthrough`.
        assert_eq!(
            parse(
                r#"{"type":"shell.overlay","rects":[
                    {"x":0,"y":0,"width":300,"height":120},
                    {"x":0,"y":0,"width":1920,"height":30,"passthrough":true}]}"#
            ),
            Request::ShellOverlay {
                rects: vec![
                    OverlayRect {
                        x: 0,
                        y: 0,
                        width: 300,
                        height: 120,
                        passthrough: false,
                    },
                    OverlayRect {
                        x: 0,
                        y: 0,
                        width: 1920,
                        height: 30,
                        passthrough: true,
                    },
                ],
            }
        );
    }

    #[test]
    fn absent_fullscreen_means_windowed() {
        assert_eq!(
            parse(r#"{"type":"view.fullscreen","id":7}"#),
            Request::ViewFullscreen {
                id: 7,
                fullscreen: false
            }
        );
    }

    #[test]
    fn the_shells_workspace_list_parses_as_it_sends_it() {
        // Field for field what data/shell/outputs.js puts on the wire: a
        // workspace on screen, and one that is not. What is omitted matters as
        // much as what is there — the shell sends neither `active` nor
        // `urgent` for a workspace that is neither, and a default that came out
        // wrong would publish every workspace as active to every bar watching.
        let Request::WorkspaceList { workspaces } = parse(
            r#"{"type":"workspace.list","workspaces":[
                 {"id":"1","name":"1","output":"DP-1","active":true},
                 {"id":"4","name":"4","output":"DP-1","hidden":true}]}"#,
        ) else {
            panic!("should be a workspace list");
        };
        assert_eq!(
            workspaces,
            vec![
                Workspace {
                    id: "1".into(),
                    name: "1".into(),
                    output: Some("DP-1".into()),
                    active: true,
                    urgent: false,
                    hidden: false,
                },
                Workspace {
                    id: "4".into(),
                    name: "4".into(),
                    output: Some("DP-1".into()),
                    active: false,
                    urgent: false,
                    hidden: true,
                },
            ]
        );
    }

    #[test]
    fn absent_hdr_enabled_is_a_toggle_not_a_disable() {
        assert_eq!(
            parse(r#"{"type":"output.hdr"}"#),
            Request::OutputHdr {
                name: None,
                enabled: None
            }
        );
    }

    #[test]
    fn unit_variants_need_no_fields() {
        assert_eq!(parse(r#"{"type":"quit"}"#), Request::Quit);
        assert_eq!(parse(r#"{"type":"view.query"}"#), Request::ViewQuery);
        assert_eq!(
            parse(r#"{"type":"output.confirm"}"#),
            Request::OutputConfirm
        );
    }

    #[test]
    fn layout_geometry_is_flattened_not_nested() {
        let request =
            parse(r#"{"type":"view.layout","id":3,"x":10,"y":20,"width":800,"height":600}"#);
        let Request::ViewLayout(layout) = request else {
            panic!("wrong variant");
        };
        assert_eq!(layout.id, 3);
        assert_eq!(
            layout.resolve(Box::new(0, 0, 1, 1)).unwrap().box_,
            Box::new(10, 20, 800, 600)
        );
    }

    #[test]
    fn absent_floating_means_tiled() {
        // A shell that says nothing about stacking gets the old behaviour,
        // which is the tiled one.
        let Request::ViewLayout(layout) =
            parse(r#"{"type":"view.layout","id":3,"x":0,"y":0,"width":8,"height":8}"#)
        else {
            panic!("wrong variant");
        };
        assert!(!layout.floating);

        let Request::ViewLayout(layout) = parse(
            r#"{"type":"view.layout","id":3,"x":0,"y":0,"width":8,"height":8,"floating":true}"#,
        ) else {
            panic!("wrong variant");
        };
        assert!(layout.floating);
    }

    #[test]
    fn layout_keeps_current_geometry_for_absent_fields() {
        let Request::ViewLayout(layout) = parse(r#"{"type":"view.layout","id":3,"width":800}"#)
        else {
            panic!("wrong variant");
        };
        let resolved = layout.resolve(Box::new(10, 20, 300, 400)).unwrap();
        assert_eq!(resolved.box_, Box::new(10, 20, 800, 400));
        assert_eq!(resolved.scale, 1.0);
        assert_eq!(resolved.clip, None);
    }

    #[test]
    fn degenerate_layout_is_dropped() {
        let Request::ViewLayout(layout) = parse(r#"{"type":"view.layout","id":3,"width":0}"#)
        else {
            panic!("wrong variant");
        };
        assert!(layout.resolve(Box::new(0, 0, 100, 100)).is_none());
    }

    #[test]
    fn out_of_range_scale_falls_back_to_one() {
        for raw in ["0", "-0.5", "1.5"] {
            let Request::ViewLayout(layout) =
                parse(&format!(r#"{{"type":"view.layout","id":3,"scale":{raw}}}"#))
            else {
                panic!("wrong variant");
            };
            assert_eq!(layout.resolve(Box::new(0, 0, 10, 10)).unwrap().scale, 1.0);
        }
    }

    #[test]
    fn clip_falls_back_to_the_new_layout_box() {
        let Request::ViewLayout(layout) = parse(
            r#"{"type":"view.layout","id":3,"x":5,"y":5,"width":100,"height":100,"clip":{"height":40}}"#,
        ) else {
            panic!("wrong variant");
        };
        let resolved = layout.resolve(Box::new(0, 0, 1, 1)).unwrap();
        assert_eq!(resolved.clip, Some(Box::new(5, 5, 100, 40)));
    }

    #[test]
    fn output_configure_nests_mode() {
        let Request::OutputConfigure(config) = parse(
            r#"{"type":"output.configure","name":"DP-1","mode":{"width":2560,"height":1440,"refresh":143998},"scale":1.25,"transform":"flipped-90"}"#,
        ) else {
            panic!("wrong variant");
        };
        assert_eq!(config.name, "DP-1");
        assert_eq!(
            config.mode,
            Some(ModeRequest {
                width: 2560,
                height: 1440,
                refresh: 143998
            })
        );
        assert_eq!(config.scale, Some(1.25));
        assert_eq!(config.transform, Some(Transform::Flipped90));
        assert_eq!(config.enabled, None);
        assert_eq!(config.x, None);
    }

    #[test]
    fn every_dispatch_table_entry_parses() {
        // The full table from src/ipc.c:1415, so a renamed variant fails here
        // rather than silently going unhandled at runtime.
        let table = [
            r#"{"type":"view.layout","id":1}"#,
            r#"{"type":"view.visible","id":1}"#,
            r#"{"type":"view.fullscreen","id":1}"#,
            r#"{"type":"view.focus","id":1}"#,
            r#"{"type":"view.close","id":1}"#,
            r#"{"type":"view.opacity","id":1}"#,
            r#"{"type":"view.query"}"#,
            r#"{"type":"shell.focus"}"#,
            r#"{"type":"shell.overview","active":true}"#,
            r#"{"type":"session.save","state":"{}"}"#,
            r#"{"type":"session.query"}"#,
            r#"{"type":"notification.action","id":1,"action":"open"}"#,
            r#"{"type":"notification.dismiss","id":1}"#,
            r#"{"type":"notification.expire","id":1}"#,
            r#"{"type":"output.configure","name":"DP-1"}"#,
            r#"{"type":"output.hdr","name":"DP-1","enabled":true}"#,
            r#"{"type":"output.confirm"}"#,
            r#"{"type":"output.active","name":"DP-1"}"#,
            r#"{"type":"output.query"}"#,
            r#"{"type":"output.test_add"}"#,
            r#"{"type":"output.test_remove"}"#,
            r#"{"type":"bind.add","chord":"Mod4+a","action":"focus.parent"}"#,
            r#"{"type":"quit"}"#,
        ];
        assert_eq!(table.len(), 23, "the C dispatch table has 23 rows");
        for json in table {
            serde_json::from_str::<Request>(json).unwrap_or_else(|e| panic!("{json}: {e}"));
        }
    }

    /// A tray click names an item, a button and where the pointer was, and a
    /// scroll names how far it turned. Both default rather than fail on the
    /// fields a terse shell leaves out.
    #[test]
    fn tray_messages_carry_the_item_and_the_pointer() {
        let Request::TrayActivate {
            id, button, x, y, ..
        } = parse(
            r#"{"type":"tray.activate","id":":1.42/StatusNotifierItem","button":"secondary","x":40,"y":8}"#,
        )
        else {
            panic!("not a tray.activate message");
        };
        assert_eq!(
            (id.as_str(), button.as_str(), x, y),
            (":1.42/StatusNotifierItem", "secondary", 40, 8)
        );

        // Everything but the item omitted: a primary click at the origin.
        let Request::TrayActivate { button, x, y, .. } =
            parse(r#"{"type":"tray.activate","id":"a/b"}"#)
        else {
            panic!("not a tray.activate message");
        };
        assert_eq!((button.as_str(), x, y), ("", 0, 0));

        let Request::TrayScroll {
            delta, orientation, ..
        } = parse(r#"{"type":"tray.scroll","id":"a/b","delta":-1}"#)
        else {
            panic!("not a tray.scroll message");
        };
        assert_eq!((delta, orientation.as_str()), (-1, ""));
    }

    #[test]
    fn status_volume_names_a_node_and_what_to_do_to_it() {
        let Request::StatusVolume {
            target,
            delta,
            mute,
        } = parse(r#"{"type":"status.volume","target":"sink","delta":5}"#)
        else {
            panic!("not a status.volume message");
        };
        assert_eq!(target, "sink");
        assert_eq!(delta, Some(5));
        assert!(!mute, "a volume change is not a mute");

        // Down, and the microphone rather than the speakers.
        let Request::StatusVolume { target, delta, .. } =
            parse(r#"{"type":"status.volume","target":"source","delta":-5}"#)
        else {
            panic!("not a status.volume message");
        };
        assert_eq!((target.as_str(), delta), ("source", Some(-5)));

        // Mute alone, which names no delta at all.
        let Request::StatusVolume { delta, mute, .. } =
            parse(r#"{"type":"status.volume","target":"sink","mute":true}"#)
        else {
            panic!("not a status.volume message");
        };
        assert_eq!(delta, None);
        assert!(mute);
    }

    #[test]
    fn status_refresh_is_a_unit_request() {
        // Not in the C-parity table: this request has no counterpart in the C
        // build, and adding a row would make that table's count assertion mean
        // something else.
        assert!(matches!(
            parse(r#"{"type":"status.refresh"}"#),
            Request::StatusRefresh
        ));
    }

    #[test]
    fn a_shell_command_carries_its_arguments_and_needs_none() {
        // Deliberately not in the table above. That one is parity with the C
        // build's dispatch, and this request has no counterpart there — adding
        // a row would make its count assertion mean something else.
        let Request::ShellCommand { command, args } =
            parse(r#"{"type":"shell.command","command":"output.focus","args":["right"]}"#)
        else {
            panic!("not a shell command");
        };
        assert_eq!(command, "output.focus");
        assert_eq!(args, ["right"]);

        // Most verbs take none, and writing `"args": []` for every one of them
        // is the sort of thing a caller gets wrong once and then debugs.
        let Request::ShellCommand { command, args } =
            parse(r#"{"type":"shell.command","command":"layout.overview"}"#)
        else {
            panic!("not a shell command");
        };
        assert_eq!(command, "layout.overview");
        assert!(args.is_empty());
    }

    #[test]
    fn shell_exec_carries_the_command_line() {
        // Not in the C-parity table above: this request has no counterpart in
        // the C build, and adding a row would make that table's count
        // assertion mean something else.
        let Request::ShellExec { command } =
            parse(r#"{"type":"shell.exec","command":"wpctl set-volume @DEFAULT_AUDIO_SINK@ 5%+"}"#)
        else {
            panic!("not a shell.exec message");
        };
        assert_eq!(command, "wpctl set-volume @DEFAULT_AUDIO_SINK@ 5%+");
    }

    #[test]
    fn config_gaps_carries_the_inner_value() {
        // Not in the C-parity table above: this request has no counterpart in
        // the C build, and adding a row would make that table's count
        // assertion mean something else.
        let Request::ConfigGaps {
            inner,
            outer,
            smart,
        } = parse(r#"{"type":"config.gaps","inner":15,"outer":4,"smart":true}"#)
        else {
            panic!("not a config.gaps message");
        };
        assert_eq!(inner, Some(15));
        assert_eq!(outer, Some(4));
        assert_eq!(smart, Some(true));

        // A gap of zero is a deliberate request (no spacing at all).
        let Request::ConfigGaps { inner, .. } = parse(r#"{"type":"config.gaps","inner":0}"#) else {
            panic!("not a config.gaps message");
        };
        assert_eq!(inner, Some(0));

        // Every field is optional, so a bare message changes nothing.
        let Request::ConfigGaps {
            inner,
            outer,
            smart,
        } = parse(r#"{"type":"config.gaps"}"#)
        else {
            panic!("not a config.gaps message");
        };
        assert_eq!(inner, None);
        assert_eq!(outer, None);
        assert_eq!(smart, None);
    }

    #[test]
    fn config_border_carries_the_radius() {
        let Request::ConfigBorder {
            radius,
            width,
            smart,
        } = parse(r#"{"type":"config.border","radius":12,"width":3,"smart":true}"#)
        else {
            panic!("not a config.border message");
        };
        assert_eq!(radius, Some(12));
        assert_eq!(width, Some(3));
        assert_eq!(smart, Some(true));

        // Square corners and no border at all, both asked for on purpose.
        let Request::ConfigBorder { radius, width, .. } =
            parse(r#"{"type":"config.border","radius":0,"width":0}"#)
        else {
            panic!("not a config.border message");
        };
        assert_eq!((radius, width), (Some(0), Some(0)));

        // Absent is not false: an absent `smart` in the config follows
        // `gaps.smart`, and a message that omits it changes nothing.
        let Request::ConfigBorder {
            radius,
            width,
            smart,
        } = parse(r#"{"type":"config.border"}"#)
        else {
            panic!("not a config.border message");
        };
        assert_eq!((radius, width, smart), (None, None, None));
    }

    #[test]
    fn config_wallpaper_carries_the_picture_and_the_fitting() {
        let Request::ConfigWallpaper { path, mode } =
            parse(r#"{"type":"config.wallpaper","path":"/pic/wall.png","mode":"tile"}"#)
        else {
            panic!("not a config.wallpaper message");
        };
        assert_eq!(path.as_deref(), Some("/pic/wall.png"));
        assert_eq!(mode.as_deref(), Some("tile"));

        // Either half alone: the mode is switched while trying a picture out,
        // and swapping the picture keeps the mode.
        let Request::ConfigWallpaper { path, mode } =
            parse(r#"{"type":"config.wallpaper","mode":"center"}"#)
        else {
            panic!("not a config.wallpaper message");
        };
        assert_eq!((path, mode.as_deref()), (None, Some("center")));

        // And the empty string, which is the only way back to the shell's own
        // background — distinct from an absent `path`, which changes nothing.
        let Request::ConfigWallpaper { path, .. } =
            parse(r#"{"type":"config.wallpaper","path":""}"#)
        else {
            panic!("not a config.wallpaper message");
        };
        assert_eq!(path.as_deref(), Some(""));
    }

    #[test]
    fn a_layout_carries_whether_the_window_was_drawn_square() {
        // Smart radius. The shell knows which window is alone on its
        // workspace and the compositor does not, so the answer travels with
        // the rectangle rather than being worked out again on the other side.
        let Request::ViewLayout(layout) = parse(
            r#"{"type":"view.layout","id":3,"x":0,"y":0,"width":100,"height":100,"square":true}"#,
        ) else {
            panic!("not a view.layout message");
        };
        assert!(layout.square);

        // Absent is a window with the configured corner, which is most of
        // them and every window an older shell sends.
        let Request::ViewLayout(layout) =
            parse(r#"{"type":"view.layout","id":3,"x":0,"y":0,"width":100,"height":100}"#)
        else {
            panic!("not a view.layout message");
        };
        assert!(!layout.square);
    }
}
