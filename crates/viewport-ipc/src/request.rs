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

    #[serde(rename = "quit")]
    Quit,
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
}
