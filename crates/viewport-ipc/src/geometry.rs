// SPDX-License-Identifier: MIT
//
// Ports the geometry pieces of src/ipc.c: the box the shell measures out of the
// DOM, and the output transform names it spells.

use serde::{Deserialize, Serialize};

/// A rectangle in output-layout coordinates.
///
/// Mirrors `struct wlr_box`. Every field is required here because a `Box` is
/// only ever constructed once the optional per-field defaults in a request have
/// already been resolved against the view's current geometry — see
/// [`crate::request::ViewLayout`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Box {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Box {
    pub const fn new(x: i32, y: i32, width: i32, height: i32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// The C build drops a layout whose extent is not positive rather than
    /// committing a degenerate box (`src/ipc.c:839`).
    pub const fn is_valid(&self) -> bool {
        self.width > 0 && self.height > 0
    }
}

/// A rectangle whose fields are all optional, as it arrives on the wire.
///
/// `object_int(object, "x", toplevel->box.x)` in the C build means "absent
/// leaves the current value alone", so absence has to survive parsing rather
/// than collapsing to zero.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartialBox {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<i32>,
}

impl PartialBox {
    /// Fill the absent fields from `current`.
    pub fn resolve(&self, current: Box) -> Box {
        Box {
            x: self.x.unwrap_or(current.x),
            y: self.y.unwrap_or(current.y),
            width: self.width.unwrap_or(current.width),
            height: self.height.unwrap_or(current.height),
        }
    }
}

/// Output transform, spelled the way `transform_name()` spells it in
/// `src/ipc.c:646`. These strings are load-bearing: the shell's monitor
/// settings compare against them literally.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Transform {
    #[default]
    #[serde(rename = "normal")]
    Normal,
    #[serde(rename = "90")]
    _90,
    #[serde(rename = "180")]
    _180,
    #[serde(rename = "270")]
    _270,
    #[serde(rename = "flipped")]
    Flipped,
    #[serde(rename = "flipped-90")]
    Flipped90,
    #[serde(rename = "flipped-180")]
    Flipped180,
    #[serde(rename = "flipped-270")]
    Flipped270,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_box_absence_keeps_current() {
        let current = Box::new(10, 20, 300, 400);
        let partial: PartialBox = serde_json::from_str(r#"{"width":500}"#).unwrap();
        assert_eq!(partial.resolve(current), Box::new(10, 20, 500, 400));
    }

    #[test]
    fn partial_box_explicit_zero_is_not_absence() {
        let current = Box::new(10, 20, 300, 400);
        let partial: PartialBox = serde_json::from_str(r#"{"x":0}"#).unwrap();
        assert_eq!(partial.resolve(current), Box::new(0, 20, 300, 400));
    }

    #[test]
    fn degenerate_box_is_rejected() {
        assert!(!Box::new(0, 0, 0, 100).is_valid());
        assert!(!Box::new(0, 0, 100, -1).is_valid());
        assert!(Box::new(0, 0, 1, 1).is_valid());
    }

    #[test]
    fn transform_names_match_the_c_build() {
        let cases = [
            (Transform::Normal, "\"normal\""),
            (Transform::_90, "\"90\""),
            (Transform::_180, "\"180\""),
            (Transform::_270, "\"270\""),
            (Transform::Flipped, "\"flipped\""),
            (Transform::Flipped90, "\"flipped-90\""),
            (Transform::Flipped180, "\"flipped-180\""),
            (Transform::Flipped270, "\"flipped-270\""),
        ];
        for (value, json) in cases {
            assert_eq!(serde_json::to_string(&value).unwrap(), json);
            assert_eq!(serde_json::from_str::<Transform>(json).unwrap(), value);
        }
    }
}
