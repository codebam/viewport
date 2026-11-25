// SPDX-License-Identifier: GPL-3.0-or-later
//
// How evenly the shell painted, out of the instants its frames arrived at.
//
// A count per second cannot tell a steady desktop from a lurching one, and the
// smoothness table in `docs/benchmarks.md` ends with the sentence that made this
// file: a steady 26 frames a second and a 26 that stalls for 200 ms once a
// second are the same number in a per-second bucket, and the difference between
// them is the one a person calls stutter. So the arrival instant of each frame
// is kept, and the interval between consecutive ones is what gets reported.
//
// The definitions are fixed, because a number that two people define differently
// is not a measurement:
//
//   gap     the interval between two consecutive frames;
//   hitch   a gap over twice the panel's period, and never under 50 ms. Two
//           frames missed is what a person notices; the floor is there because
//           two periods of a slow panel is not much of a miss, and 50 ms is
//           where any panel's cadence starts to be a gap rather than a jitter;
//   stall   a gap over 250 ms, long enough that the desktop froze rather than
//           dropped a frame.
//
// A window — one tick of the compositor's housekeeping — reports the frames that
// arrived in it, the p50, p95 and max of the gaps that *ended* in it, and the
// two counts. A gap belongs to the window its later frame landed in, which is
// the only attribution that cannot miss one: the pause before the shell starts
// painting again crosses a window boundary wherever the boundary is, and a
// metric that dropped those would report the worst stalls as nothing at all.
// The price is one long gap per return from idle, visible as a `max` out of all
// proportion to the rest rather than as a plausible number. A window in which
// fewer than two frames arrived reports no gaps at all: an idle desk's interval
// says nothing about pacing, and it is exactly the gap that must not be counted
// as a hitch.
//
// Nothing here gates itself. `ViewportState::note_shell_frame` is where the
// `VIEWPORT_SHELL_RATE` check lives, so a desktop nobody is measuring never
// fills this ring and never reads a clock for it.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// A gap over this is a hitch whatever the panel's period is.
pub const HITCH_FLOOR: Duration = Duration::from_millis(50);

/// A gap over this is a stall: the desktop visibly froze.
pub const STALL: Duration = Duration::from_millis(250);

/// How many arrival instants are kept.
///
/// Eight seconds at 240 Hz, and half a minute at 60. A window is one second, so
/// the ring is never the reason a window is short; it exists so that a shell
/// left painting for hours cannot grow this without bound.
const RING: usize = 2048;

/// The gap over which a missing frame becomes a hitch, for this period.
pub fn hitch_threshold(period: Duration) -> Duration {
    (period * 2).max(HITCH_FLOOR)
}

/// What one window of shell frames came to.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ShellGapStats {
    /// How many frames arrived in the window.
    pub count: usize,
    /// The gaps that ended in it, when there were enough frames for them to
    /// say anything.
    pub gaps: Option<GapSummary>,
}

/// The gaps of one window, summarised.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GapSummary {
    /// Median gap, in milliseconds.
    pub p50_ms: f64,
    /// 95th percentile gap, in milliseconds.
    pub p95_ms: f64,
    /// Longest gap, in milliseconds.
    pub max_ms: f64,
    /// Gaps over [`hitch_threshold`].
    pub hitches: usize,
    /// Gaps over [`STALL`].
    pub stalls: usize,
}

/// The arrival instants of recent shell frames, and nothing else.
#[derive(Debug, Default)]
pub struct ShellPacing {
    frames: VecDeque<Instant>,
}

impl ShellPacing {
    /// Note that the shell handed over a frame just now.
    pub fn record(&mut self, at: Instant) {
        if self.frames.len() == RING {
            self.frames.pop_front();
        }
        self.frames.push_back(at);
    }

    /// Forget every instant, for a shell that is no longer the one painting.
    ///
    /// A restart is not a frame gap: the wall clock it took is in the restart
    /// line that caused it, and leaving the old page's instants in the ring
    /// would report the respawn as a stall, which is a pacing number no pacing
    /// decision can act on.
    pub fn clear(&mut self) {
        self.frames.clear();
    }

    /// The gaps that ended in `(since, now]`, or `None` when too little
    /// arrived for their statistics to mean anything.
    pub fn window(&self, since: Instant, now: Instant, period: Duration) -> ShellGapStats {
        let mut count = 0usize;
        let mut gaps: Vec<Duration> = Vec::new();
        let mut previous: Option<Instant> = None;
        for &at in &self.frames {
            if at > now {
                break;
            }
            if at > since {
                count += 1;
                // The first frame of the window carries the gap that ended in
                // it, whose other end may be the last frame of the window
                // before. See the file's header.
                if let Some(before) = previous {
                    gaps.push(at.saturating_duration_since(before));
                }
            }
            previous = Some(at);
        }
        // One frame is not a rate, and the interval before it may be an idle
        // desk rather than a pause in painting.
        if count < 2 {
            return ShellGapStats { count, gaps: None };
        }

        let mut ms: Vec<f64> = gaps.iter().map(|gap| gap.as_secs_f64() * 1e3).collect();
        ms.sort_by(f64::total_cmp);
        let threshold = hitch_threshold(period);
        ShellGapStats {
            count,
            gaps: Some(GapSummary {
                p50_ms: percentile(&ms, 0.50),
                p95_ms: percentile(&ms, 0.95),
                max_ms: ms.last().copied().unwrap_or(0.0),
                hitches: gaps.iter().filter(|gap| **gap > threshold).count(),
                stalls: gaps.iter().filter(|gap| **gap > STALL).count(),
            }),
        }
    }
}

/// The p-th percentile of an ascending list, interpolating between neighbours.
///
/// Linear interpolation rather than nearest-rank, because every reader of this
/// number compares it against another one: nearest-rank pins p95 to the maximum
/// for any window of fewer than twenty frames, and a p95 that cannot differ from
/// `max` says nothing `max` does not already say.
fn percentile(sorted: &[f64], p: f64) -> f64 {
    let Some(last) = sorted.len().checked_sub(1) else {
        return 0.0;
    };
    let rank = last as f64 * p;
    let lower = rank.floor() as usize;
    let upper = rank.ceil() as usize;
    let weight = rank - lower as f64;
    sorted[lower] * (1.0 - weight) + sorted[upper] * weight
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 60 Hz panel, which is the period every test here measures against
    /// unless it says otherwise.
    const PERIOD: Duration = Duration::from_micros(16_667);

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    /// Ten gaps, 10 ms apart and widening, so that every percentile lands on a
    /// value this test can write down.
    #[test]
    fn percentiles_and_max_over_a_known_sequence() {
        let base = Instant::now();
        let mut pacing = ShellPacing::default();
        let mut previous = 0;
        pacing.record(at(base, 0));
        for step in 1..=10u64 {
            // 10, 20, 30 … 100 ms between consecutive frames.
            previous += step * 10;
            pacing.record(at(base, previous));
        }

        // Gaps 10..100 ms, so 10 gaps and 11 frames. The window starts one
        // millisecond before the first frame so that the first gap is internal
        // to it.
        let stats = pacing.window(
            base - Duration::from_millis(1),
            base + Duration::from_secs(1),
            PERIOD,
        );
        assert_eq!(stats.count, 11);
        let gaps = stats.gaps.expect("eleven frames is more than enough");
        // Interpolated: 55 is between the fifth gap (50) and the sixth (60),
        // and 95.5 between the ninth (90) and the tenth (100).
        assert!((gaps.p50_ms - 55.0).abs() < 1e-9, "p50 {}", gaps.p50_ms);
        assert!((gaps.p95_ms - 95.5).abs() < 1e-9, "p95 {}", gaps.p95_ms);
        assert!((gaps.max_ms - 100.0).abs() < 1e-9, "max {}", gaps.max_ms);
        // Twice 16.667 ms is under the floor, so the hitch threshold is 50 ms
        // and the gaps of 60 and over are the hitches. Exactly 50 is not one:
        // the comparison is strictly greater.
        assert_eq!(gaps.hitches, 5);
        assert_eq!(gaps.stalls, 0);
    }

    /// Both thresholds, on each side, including the two arms of the hitch
    /// threshold and the strictly-greater comparisons.
    #[test]
    fn the_two_thresholds() {
        // Twice a fast panel's period is under the floor; twice a slow one's is
        // over it. The floor is not a second rule, it is the minimum of one.
        assert_eq!(hitch_threshold(PERIOD), Duration::from_millis(50));
        assert_eq!(
            hitch_threshold(Duration::from_millis(150)),
            Duration::from_millis(300)
        );

        let base = Instant::now();
        let mut pacing = ShellPacing::default();
        // Gaps of exactly 50, 250 and 251 ms.
        for at_ms in [0, 50, 300, 551] {
            pacing.record(at(base, at_ms));
        }
        let stats = pacing.window(
            base - Duration::from_millis(1),
            base + Duration::from_secs(2),
            PERIOD,
        );
        assert_eq!(stats.count, 4);
        let gaps = stats.gaps.expect("four frames");
        assert!((gaps.max_ms - 251.0).abs() < 1e-9, "max {}", gaps.max_ms);
        // 50 is not over 50; 250 and 251 are.
        assert_eq!(gaps.hitches, 2);
        // 250 is not over 250; 251 is. That is the whole difference between a
        // desktop that dropped frames and one that froze.
        assert_eq!(gaps.stalls, 1);

        // The same gaps against a 5 Hz panel: 50 ms is well inside a period, so
        // none of them is a hitch at all.
        let slow = pacing
            .window(
                base - Duration::from_millis(1),
                base + Duration::from_secs(2),
                Duration::from_millis(200),
            )
            .gaps
            .expect("four frames");
        assert_eq!(slow.hitches, 0);
        assert_eq!(slow.stalls, 1);
    }

    /// A desk nobody is touching: no frames, or one, and no gap statistics to
    /// misread as a hitch.
    #[test]
    fn an_idle_desk_is_not_a_hitch() {
        let base = Instant::now();
        let mut pacing = ShellPacing::default();
        pacing.record(at(base, 0));
        pacing.record(at(base, 16));

        // Three seconds in which it painted nothing at all.
        let quiet = pacing.window(at(base, 16), at(base, 3000), PERIOD);
        assert_eq!(quiet.count, 0);
        assert_eq!(quiet.gaps, None, "an empty window has no gaps to report");

        // And the second in which it came back and painted once. The pause
        // before that frame is the idle desk, not a hitch.
        pacing.record(at(base, 3000));
        let waking = pacing.window(at(base, 16), at(base, 3016), PERIOD);
        assert_eq!(waking.count, 1);
        assert_eq!(waking.gaps, None, "one frame is not a rate");
    }

    /// A stall that straddles a window boundary is reported once, not twice and
    /// not never.
    #[test]
    fn a_stall_across_a_boundary_is_reported_once() {
        let base = Instant::now();
        let mut pacing = ShellPacing::default();
        // Sixty frames at 16 ms, then a 300 ms pause, then painting resumes.
        for step in 0..=60u64 {
            pacing.record(at(base, step * 16));
        }
        for at_ms in [1260, 1276, 1292] {
            pacing.record(at(base, at_ms));
        }

        // The window the pause began in has sixty frames and no gap over a
        // frame's length in it.
        let before = pacing.window(base - Duration::from_millis(1), at(base, 960), PERIOD);
        assert_eq!(before.count, 61);
        let before = before.gaps.expect("sixty frames");
        assert_eq!(before.hitches, 0);
        assert_eq!(before.stalls, 0);

        // The one it ended in has the stall, and one of it.
        let after = pacing.window(at(base, 960), at(base, 2000), PERIOD);
        assert_eq!(after.count, 3);
        let after = after.gaps.expect("three frames");
        assert_eq!(after.stalls, 1);
        assert_eq!(after.hitches, 1);
        assert!((after.max_ms - 300.0).abs() < 1e-9, "max {}", after.max_ms);
    }

    /// The ring is bounded, and drops the oldest frames rather than growing.
    #[test]
    fn the_ring_is_bounded() {
        let base = Instant::now();
        let mut pacing = ShellPacing::default();
        for step in 0..(RING as u64 + 100) {
            pacing.record(at(base, step));
        }
        assert_eq!(pacing.frames.len(), RING);
        // A window the dropped frames would have covered now starts partway
        // through, which is the one cost of the bound.
        let stats = pacing.window(base - Duration::from_millis(1), at(base, 4000), PERIOD);
        assert_eq!(stats.count, RING);
    }

    /// Clearing means the next window starts from nothing, which is what a
    /// restarted shell is.
    #[test]
    fn a_cleared_ring_has_no_history() {
        let base = Instant::now();
        let mut pacing = ShellPacing::default();
        pacing.record(at(base, 0));
        pacing.clear();
        pacing.record(at(base, 5000));
        let stats = pacing.window(base - Duration::from_millis(1), at(base, 6000), PERIOD);
        assert_eq!(stats.count, 1);
        assert_eq!(
            stats.gaps, None,
            "the frame before the restart is not evidence about this one"
        );
    }
}
