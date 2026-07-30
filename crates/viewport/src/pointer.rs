// SPDX-License-Identifier: GPL-3.0-or-later
//
// Pointer capture, for games. Ports src/pointer.c.
//
// A first-person game needs two things a desktop pointer does not provide:
//
//   relative motion  Mouselook is driven by how far the mouse moved, not where
//                    the cursor ended up. An absolute position saturates the
//                    moment the cursor reaches a screen edge, which is why a
//                    game without this can only turn so far before stopping.
//
//   a constraint     The cursor must stop moving — locked in place, or
//                    confined to a region — so it neither escapes onto the
//                    other monitor mid-fight nor generates absolute motion the
//                    game would misread.
//
// Both are separate Wayland protocols and they work together: while a lock is
// active the compositor stops moving its cursor entirely and the client is
// driven purely by relative deltas.
//
// This also covers X11 games under Xwayland. XGrabPointer has no direct
// equivalent, so Xwayland implements it by taking out exactly these two
// protocols on the client's behalf — a game under Proton and a native Wayland
// one arrive at the same code.

use smithay::utils::{Logical, Point, Rectangle};

/// Pull a point back inside a region, if it has left it.
///
/// Returns `None` when the point is already inside, or when there is nothing
/// to confine it to — an empty region means the whole surface, per the
/// protocol, and there is no edge to snap to.
///
/// Rectangles are half-open, so the far edges sit one pixel short of the
/// bound: a point at exactly `x + width` is outside.
pub fn confine(
    region: &[Rectangle<i32, Logical>],
    at: Point<f64, Logical>,
) -> Option<Point<f64, Logical>> {
    if region.is_empty() {
        return None;
    }
    if region.iter().any(|rect| contains(*rect, at)) {
        return None;
    }

    // The nearest point on the nearest rectangle. With several, the closest
    // wins — snapping to the first would jump the cursor across the surface
    // when a region is drawn as two boxes.
    let mut best: Option<(f64, Point<f64, Logical>)> = None;
    for rect in region {
        let snapped = nearest(*rect, at);
        let distance = (snapped.x - at.x).powi(2) + (snapped.y - at.y).powi(2);
        if best.is_none_or(|(d, _)| distance < d) {
            best = Some((distance, snapped));
        }
    }
    best.map(|(_, point)| point)
}

fn contains(rect: Rectangle<i32, Logical>, at: Point<f64, Logical>) -> bool {
    at.x >= rect.loc.x as f64
        && at.y >= rect.loc.y as f64
        && at.x < (rect.loc.x + rect.size.w) as f64
        && at.y < (rect.loc.y + rect.size.h) as f64
}

fn nearest(rect: Rectangle<i32, Logical>, at: Point<f64, Logical>) -> Point<f64, Logical> {
    // One short of the far edge, because the rectangle is half-open and a
    // point on the bound itself would be outside again.
    let max_x = (rect.loc.x + rect.size.w) as f64 - 1.0;
    let max_y = (rect.loc.y + rect.size.h) as f64 - 1.0;
    Point::from((
        at.x.clamp(rect.loc.x as f64, max_x.max(rect.loc.x as f64)),
        at.y.clamp(rect.loc.y as f64, max_y.max(rect.loc.y as f64)),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, Logical> {
        Rectangle::new((x, y).into(), (w, h).into())
    }

    #[test]
    fn a_point_inside_is_left_alone() {
        let region = [rect(0, 0, 100, 100)];
        assert_eq!(confine(&region, (50.0, 50.0).into()), None);
    }

    #[test]
    fn an_empty_region_confines_nothing() {
        // The protocol says an empty region means the whole surface, so there
        // is no edge to snap to and the pointer is not the compositor's to
        // move.
        assert_eq!(confine(&[], (5000.0, 5000.0).into()), None);
    }

    #[test]
    fn a_point_outside_is_pulled_to_the_edge() {
        let region = [rect(0, 0, 100, 100)];
        // Half-open: the far edge is 99, not 100. A point left at exactly 100
        // is outside again on the next test and the cursor jitters on the
        // boundary.
        assert_eq!(
            confine(&region, (150.0, 50.0).into()),
            Some(Point::from((99.0, 50.0)))
        );
        assert_eq!(
            confine(&region, (-10.0, -10.0).into()),
            Some(Point::from((0.0, 0.0)))
        );
    }

    #[test]
    fn the_point_that_was_snapped_to_is_then_inside() {
        // Otherwise confinement never settles.
        let region = [rect(0, 0, 100, 100)];
        let snapped = confine(&region, (150.0, 150.0).into()).expect("outside");
        assert_eq!(
            confine(&region, snapped),
            None,
            "snapped to a point still outside"
        );
    }

    #[test]
    fn the_nearest_of_several_rectangles_wins() {
        // A region drawn as two boxes — a map widget with a gap, say.
        // Snapping to whichever came first would throw the cursor across the
        // surface.
        let region = [rect(0, 0, 10, 10), rect(500, 0, 10, 10)];
        let snapped = confine(&region, (505.0, 50.0).into()).expect("outside");
        assert_eq!(snapped, Point::from((505.0, 9.0)), "snapped to the far box");
    }

    #[test]
    fn a_one_pixel_region_does_not_invert() {
        // max would be one short of the bound, which is behind the origin.
        // Clamping with an inverted range panics, and a region this small is
        // legal.
        let region = [rect(10, 10, 1, 1)];
        assert_eq!(
            confine(&region, (50.0, 50.0).into()),
            Some(Point::from((10.0, 10.0)))
        );
    }
}
