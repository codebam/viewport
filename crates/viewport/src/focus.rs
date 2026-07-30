// SPDX-License-Identifier: GPL-3.0-or-later
//
// Directional focus. Ports viewport_focus_direction in src/xdg_shell.c:963,
// with a different rule for choosing between candidates — see `nearest`.
//
// This is one of the few layout questions the compositor answers itself, and
// the reason is that it is not really a layout question: "the window to the
// left of this one" is about where things are on screen, which the compositor
// already knows, and it has to work across monitors where the shell's tree
// does not reach. In a scrolling layout it *is* a layout question — the column
// you want is usually scrolled off the screen — and there the binding goes to
// the shell instead.

use smithay::utils::{Logical, Rectangle};

/// Which way to look.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

impl Direction {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "left" => Some(Self::Left),
            "right" => Some(Self::Right),
            "up" => Some(Self::Up),
            "down" => Some(Self::Down),
            _ => None,
        }
    }

    fn horizontal(self) -> bool {
        matches!(self, Self::Left | Self::Right)
    }
}

/// A candidate window and the rectangle it occupies.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Candidate {
    pub id: u32,
    pub rect: Rectangle<i32, Logical>,
}

impl Candidate {
    /// Start and end along the axis of travel.
    fn span_along(&self, direction: Direction) -> (i32, i32) {
        if direction.horizontal() {
            (self.rect.loc.x, self.rect.loc.x + self.rect.size.w)
        } else {
            (self.rect.loc.y, self.rect.loc.y + self.rect.size.h)
        }
    }

    /// Start and end across it.
    fn span_across(&self, direction: Direction) -> (i32, i32) {
        if direction.horizontal() {
            (self.rect.loc.y, self.rect.loc.y + self.rect.size.h)
        } else {
            (self.rect.loc.x, self.rect.loc.x + self.rect.size.w)
        }
    }

    fn centre(&self) -> (f64, f64) {
        (
            self.rect.loc.x as f64 + self.rect.size.w as f64 / 2.0,
            self.rect.loc.y as f64 + self.rect.size.h as f64 / 2.0,
        )
    }
}

/// The window to focus when moving `direction` from `from`.
///
/// Two rules, in order.
///
/// A window is "beside" you if its span across the direction of travel
/// overlaps yours — for `right`, if it shares any rows. Those are the windows
/// a person means, and among them the nearest edge wins. This is the whole
/// rule in the ordinary case and it needs no tuning.
///
/// Only when nothing overlaps does it fall back to comparing centres, which is
/// what C does for every case (`src/xdg_shell.c:1030`) with lateral distance
/// weighted double. That weighting is a fudge: it makes a window beside you
/// usually beat one far off-axis, but "usually" is doing real work — with two
/// tall windows on the left and a short one at the bottom right, `right` from
/// the top-left window can land on a window that shares no rows with it at
/// all. Checking overlap first answers that case exactly instead of
/// approximately.
pub fn nearest(
    candidates: &[Candidate],
    from: Option<Candidate>,
    direction: Direction,
) -> Option<u32> {
    let Some(from) = from else {
        // Nothing focused, so nothing to be on the wrong side of: the nearest
        // window in the layout wins outright.
        return candidates
            .iter()
            .min_by(|a, b| {
                distance(a.centre(), (0.0, 0.0)).total_cmp(&distance(b.centre(), (0.0, 0.0)))
            })
            .map(|c| c.id);
    };

    let (from_start, from_end) = from.span_along(direction);
    let (from_near, from_far) = from.span_across(direction);
    let forward = matches!(direction, Direction::Right | Direction::Down);

    let mut beside: Option<(u32, i32)> = None;
    let mut anywhere: Option<(u32, f64)> = None;

    for candidate in candidates {
        if candidate.id == from.id {
            continue;
        }
        let (start, end) = candidate.span_along(direction);

        // The gap between the two edges that face each other. Negative means
        // the candidate is behind, or overlapping, which is not "in that
        // direction" either way.
        let gap = if forward {
            start - from_end
        } else {
            from_start - end
        };
        if gap < 0 {
            continue;
        }

        let (near, far) = candidate.span_across(direction);
        if near < from_far && from_near < far {
            // Beside: shares rows (or columns) with the focused window.
            if beside.is_none_or(|(_, best)| gap < best) {
                beside = Some((candidate.id, gap));
            }
        } else if beside.is_none() {
            let d = distance(candidate.centre(), from.centre());
            if anywhere.is_none_or(|(_, best)| d < best) {
                anywhere = Some((candidate.id, d));
            }
        }
    }

    beside
        .map(|(id, _)| id)
        .or_else(|| anywhere.map(|(id, _)| id))
}

fn distance(a: (f64, f64), b: (f64, f64)) -> f64 {
    let (dx, dy) = (a.0 - b.0, a.1 - b.1);
    dx * dx + dy * dy
}

/// The next or previous window in the order they were added.
///
/// The shell controls that order by the sequence in which it was told about
/// them, which is what the dock shows — so Tab walks the dock rather than a
/// stacking order nobody can see. Wraps rather than stopping at the ends
/// (`src/xdg_shell.c:980`).
pub fn step(ids: &[u32], focused: Option<u32>, forward: bool) -> Option<u32> {
    if ids.is_empty() {
        return None;
    }
    let Some(focused) = focused.and_then(|f| ids.iter().position(|id| *id == f)) else {
        return Some(if forward { ids[0] } else { ids[ids.len() - 1] });
    };
    let next = if forward {
        (focused + 1) % ids.len()
    } else {
        (focused + ids.len() - 1) % ids.len()
    };
    Some(ids[next])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(id: u32, x: i32, y: i32, w: i32, h: i32) -> Candidate {
        Candidate {
            id,
            rect: Rectangle::new((x, y).into(), (w, h).into()),
        }
    }

    #[test]
    fn the_window_in_that_direction_wins() {
        let windows = [win(1, 0, 0, 400, 400), win(2, 400, 0, 400, 400)];
        assert_eq!(
            nearest(&windows, Some(windows[0]), Direction::Right),
            Some(2)
        );
        assert_eq!(
            nearest(&windows, Some(windows[1]), Direction::Left),
            Some(1)
        );
    }

    #[test]
    fn nothing_behind_you_is_a_candidate() {
        // Focus does not wrap around the screen: there is no window left of
        // the leftmost one, and the answer is none rather than the far right.
        let windows = [win(1, 0, 0, 400, 400), win(2, 400, 0, 400, 400)];
        assert_eq!(nearest(&windows, Some(windows[0]), Direction::Left), None);
    }

    #[test]
    fn a_window_that_shares_no_rows_loses_to_one_that_does() {
        // The case the centre rule gets wrong. Window 3's centre is nearer in
        // x, but it sits below the focused window and shares none of its rows;
        // window 2 is what "right" means here.
        //
        // Centres: from (200,200), window 2 is 400 right / 0 off-axis, window
        // 3 is 300 right / 700 off-axis. C scores those 400 and 1700, so it
        // agrees here — but only because of the doubling. Overlap decides it
        // without a constant.
        let windows = [
            win(1, 0, 0, 400, 400),
            win(2, 400, 0, 400, 400),
            win(3, 300, 800, 400, 400),
        ];
        assert_eq!(
            nearest(&windows, Some(windows[0]), Direction::Right),
            Some(2)
        );
    }

    #[test]
    fn a_window_beside_you_wins_even_when_one_off_axis_is_nearer() {
        // Here the centre rule and the overlap rule genuinely disagree.
        // Window 3 is much closer by centre distance — 50 to the right versus
        // 900 — but it is above the focused window and shares no rows with it.
        // C would score window 3 at 50 + 2*750 = 1550 against window 2 at 900,
        // and pick window 2 as well; make the drift smaller and C flips to the
        // window that is not beside you at all.
        let windows = [
            win(1, 0, 800, 400, 400),
            win(2, 1300, 800, 400, 400),
            win(3, 450, 500, 400, 200),
        ];
        assert_eq!(
            nearest(&windows, Some(windows[0]), Direction::Right),
            Some(2),
            "the window sharing rows with the focused one"
        );
    }

    #[test]
    fn a_window_that_shares_nothing_is_still_reachable() {
        // With no overlapping candidate the rule has to fall back, or a window
        // on its own row becomes unreachable by keyboard.
        let windows = [win(1, 0, 0, 400, 400), win(2, 800, 900, 400, 400)];
        assert_eq!(
            nearest(&windows, Some(windows[0]), Direction::Right),
            Some(2)
        );
    }

    #[test]
    fn the_nearest_of_several_beside_you_wins() {
        let windows = [
            win(1, 0, 0, 400, 400),
            win(2, 400, 0, 400, 400),
            win(3, 900, 0, 400, 400),
        ];
        assert_eq!(
            nearest(&windows, Some(windows[0]), Direction::Right),
            Some(2)
        );
    }

    #[test]
    fn with_nothing_focused_a_window_is_still_chosen() {
        // Otherwise the first directional press after a focus loss does
        // nothing, which reads as a dead key.
        let windows = [win(1, 900, 0, 400, 400), win(2, 300, 0, 400, 400)];
        assert_eq!(nearest(&windows, None, Direction::Right), Some(2));
        assert_eq!(nearest(&[], None, Direction::Right), None);
    }

    #[test]
    fn tab_walks_the_order_the_shell_was_told_and_wraps() {
        let ids = [1, 2, 3];
        assert_eq!(step(&ids, Some(1), true), Some(2));
        assert_eq!(step(&ids, Some(3), true), Some(1), "wraps forward");
        assert_eq!(step(&ids, Some(1), false), Some(3), "wraps backward");
        // Nothing focused: the first going forward, the last going back.
        assert_eq!(step(&ids, None, true), Some(1));
        assert_eq!(step(&ids, None, false), Some(3));
        // A focused window no longer in the list is treated the same way.
        assert_eq!(step(&ids, Some(99), true), Some(1));
        assert_eq!(step(&[], Some(1), true), None);
    }
}
