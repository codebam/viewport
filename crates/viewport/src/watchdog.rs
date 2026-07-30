// SPDX-License-Identifier: GPL-3.0-or-later
//
// What happens when the shell stops answering. Ports src/watchdog.c.
//
// The entire layout lives in a web page. That is the point of this compositor,
// and it is also its one structural risk: a JavaScript error, a page that
// fails to load, a shell served from a machine that has gone away — any of
// them and no window is ever placed. Windows stay hidden, nothing recovers,
// and the session looks like a black screen with a working keyboard.
//
// So placement is watched. A window that maps and is not given a rectangle
// within a couple of seconds is laid out here instead, by a deliberately
// stupid tiler: every visible window gets an equal column. It is not a layout
// anyone would want, and it is not meant to be — it exists so that a broken
// shell leaves a usable desktop rather than an unusable one, long enough to
// open a terminal and fix it.
//
// The moment the shell does answer, the watchdog is disarmed and never fires
// again for that window, so a shell that is merely slow to start costs
// nothing.

use std::time::Duration;

/// Long enough that a shell fetching over the network is not cut off, short
/// enough that a broken one is not left on screen doing nothing.
pub const TIMEOUT: Duration = Duration::from_millis(2500);

/// One window in the fallback layout: which view, and the rectangle it gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub id: u32,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// Equal columns across `area`, in the order the windows were announced.
///
/// Deliberately ignores workspaces, the tiling tree and everything else the
/// shell owns: none of that is trustworthy here, because the thing that
/// maintains it is what stopped answering.
pub fn columns(ids: &[u32], area: (i32, i32, i32, i32)) -> Vec<Placement> {
    let (x, y, width, height) = area;
    if width <= 0 || height <= 0 || ids.is_empty() {
        return Vec::new();
    }

    let each = width / ids.len() as i32;
    if each <= 0 {
        // More windows than pixels. Placing them at zero width would hide
        // every one of them, which is the situation this exists to avoid.
        return Vec::new();
    }

    ids.iter()
        .enumerate()
        .map(|(index, id)| Placement {
            id: *id,
            x: x + index as i32 * each,
            y,
            width: each,
            height,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_get_equal_columns() {
        let placed = columns(&[1, 2, 3], (0, 0, 900, 600));
        assert_eq!(
            placed,
            vec![
                Placement {
                    id: 1,
                    x: 0,
                    y: 0,
                    width: 300,
                    height: 600
                },
                Placement {
                    id: 2,
                    x: 300,
                    y: 0,
                    width: 300,
                    height: 600
                },
                Placement {
                    id: 3,
                    x: 600,
                    y: 0,
                    width: 300,
                    height: 600
                },
            ]
        );
    }

    #[test]
    fn the_layout_starts_at_the_area_origin() {
        // The area is the output layout, which need not start at zero — a
        // second monitor placed left of the first has a negative x.
        let placed = columns(&[1, 2], (-2560, 0, 5120, 1440));
        assert_eq!(placed[0].x, -2560);
        assert_eq!(placed[1].x, 0);
    }

    #[test]
    fn nothing_to_place_is_not_a_layout() {
        assert!(columns(&[], (0, 0, 1920, 1080)).is_empty());
        // An output with no size yet, which is what the very first frame sees.
        assert!(columns(&[1], (0, 0, 0, 0)).is_empty());
    }

    #[test]
    fn more_windows_than_pixels_places_none() {
        // Rather than giving every window a zero-width rectangle, which hides
        // all of them — the exact outcome this is here to prevent.
        assert!(columns(&(0..2000u32).collect::<Vec<_>>(), (0, 0, 100, 100)).is_empty());
    }
}
