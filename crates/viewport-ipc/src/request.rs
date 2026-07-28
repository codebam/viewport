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
        assert_eq!(parse(r#"{"type":"output.confirm"}"#), Request::OutputConfirm);
    }

    #[test]
    fn layout_geometry_is_flattened_not_nested() {
        let request = parse(r#"{"type":"view.layout","id":3,"x":10,"y":20,"width":800,"height":600}"#);
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
        let Request::ViewLayout(layout) = parse(r#"{"type":"view.layout","id":3,"width":0}"#) else {
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
}
