// SPDX-License-Identifier: GPL-3.0-or-later
//
// Idle: locking and blanking after a while. Ports src/idle.c.
//
// Two independent deadlines measured from the last input, and neither is the
// other's business — a screen that blanks is not locked, and a session that is
// locked need not be dark. Both are off unless the config file asks for them,
// because a compositor that blanks a screen nobody asked it to blank is worse
// than one that never does.

use std::time::{Duration, Instant};

/// Which half of an input event this is, as far as a blank is concerned.
///
/// The only distinction that matters here: a release cannot have been meant to
/// wake anything, because the hand that pressed the chord has to come off it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    /// A press, a motion, a scroll — anything somebody did on purpose.
    Deliberate,
    /// A key or button coming back up.
    Release,
}

/// What the config asks for.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Settings {
    /// Seconds of inactivity before `lock_command` runs. Zero or absent is off.
    pub lock_after: Option<i64>,
    /// Seconds before the outputs are turned off. Zero or absent is off.
    pub blank_after: Option<i64>,
    pub lock_command: Option<String>,
}

impl Settings {
    /// Whether anything here needs a timer at all.
    pub fn wanted(&self) -> bool {
        self.lock_after.unwrap_or(0) > 0 || self.blank_after.unwrap_or(0) > 0
    }
}

/// What the timer decided to do this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Actions {
    pub lock: bool,
    pub blank: bool,
}

/// How long since the last input, and what has already fired.
#[derive(Debug)]
pub struct Idle {
    pub since: Instant,
    locked: bool,
    blanked: bool,
    /// Whether the chord that asked for this blank is still on its way up.
    ///
    /// Blanking is reachable from a key chord, and a chord is at least two
    /// events: the press that asks for it and the release that follows. Without
    /// this the release is activity, activity wakes the screens, and the
    /// binding appears to do nothing at all (`src/idle.c:53`).
    ///
    /// The C build waited out a stretch of wall time instead, which is the
    /// wrong quantity: for as long as it ran, keys typed at a dark screen went
    /// into whatever held focus with nothing to show for them. Releases are
    /// what has to be ignored, so releases are what is ignored — a press, a
    /// click or a moved mouse brings the screens back on the spot.
    awaiting_release: bool,
    /// Something is holding idle off — a video player, usually.
    inhibited: bool,
}

impl Default for Idle {
    fn default() -> Self {
        Self {
            since: Instant::now(),
            locked: false,
            blanked: false,
            awaiting_release: false,
            inhibited: false,
        }
    }
}

impl Idle {
    /// Input arrived, so both deadlines start again.
    ///
    /// Returns whether the screens were blanked and need bringing back, which
    /// is the one thing activity has to undo rather than merely reset.
    pub fn activity(&mut self, activity: Activity) -> bool {
        // Still the keystroke that asked for this coming back up. Nothing is
        // reset either, not only the wake suppressed: treating the chord as
        // activity would also re-arm the deadlines it just pre-empted.
        if self.blanked && self.awaiting_release && activity == Activity::Release {
            return false;
        }

        self.since = Instant::now();
        self.locked = false;
        let was_blanked = self.blanked;
        self.blanked = false;
        self.awaiting_release = false;
        was_blanked
    }

    pub fn set_inhibited(&mut self, inhibited: bool) {
        self.inhibited = inhibited;
    }

    /// Whether the screens are currently blanked.
    ///
    /// Read by the tests, which is what checks that a deadline actually
    /// blanked them rather than only setting a flag.
    #[allow(dead_code)]
    pub fn blanked(&self) -> bool {
        self.blanked
    }

    /// Mark the screens as blanked without a deadline having passed.
    ///
    /// Flagged as though the timer had done it, so the next keypress brings
    /// them back through the same path. Blanking without that leaves no way to
    /// undo it short of a deadline that has already fired
    /// (`src/idle.c:154`).
    pub fn force_blank(&mut self) {
        self.blanked = true;
        self.awaiting_release = true;
    }

    /// What to do, given how long it has been.
    ///
    /// Each fires once per idle period; `activity` is what re-arms them.
    pub fn tick(&mut self, settings: &Settings, elapsed: Duration) -> Actions {
        if self.inhibited {
            // An inhibitor counts as activity, so the countdown starts again
            // from when it is released rather than firing the instant it is
            // (`src/idle.c:124`).
            self.since = Instant::now();
            return Actions::default();
        }

        let seconds = elapsed.as_secs() as i64;
        let mut actions = Actions::default();

        if let Some(after) = settings.lock_after.filter(|a| *a > 0) {
            if !self.locked && seconds >= after {
                self.locked = true;
                actions.lock = true;
            }
        }
        if let Some(after) = settings.blank_after.filter(|a| *a > 0) {
            if !self.blanked && seconds >= after {
                self.blanked = true;
                // Deliberately not awaiting a release, though `src/idle.c:147`
                // starts its grace here. That grace exists to swallow the
                // release of the chord that asked for a blank, and a deadline
                // fires after a stretch of no input at all — there is no
                // release in flight to swallow. All it would buy here is a
                // screen that stays black through the keyboard being tidied up
                // under somebody's elbow.
                actions.blank = true;
            }
        }
        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(lock: Option<i64>, blank: Option<i64>) -> Settings {
        Settings {
            lock_after: lock,
            blank_after: blank,
            lock_command: Some("swaylock".to_owned()),
        }
    }

    #[test]
    fn nothing_happens_without_a_deadline() {
        // Off unless asked for. A compositor that blanks a screen nobody asked
        // it to blank is worse than one that never does.
        let mut idle = Idle::default();
        let none = Settings::default();
        assert!(!none.wanted());
        assert_eq!(
            idle.tick(&none, Duration::from_secs(10_000)),
            Actions::default()
        );
    }

    #[test]
    fn a_zero_deadline_is_off_rather_than_immediate() {
        let mut idle = Idle::default();
        let zero = settings(Some(0), Some(0));
        assert!(!zero.wanted());
        assert_eq!(
            idle.tick(&zero, Duration::from_secs(60)),
            Actions::default()
        );
    }

    #[test]
    fn each_deadline_fires_once_per_idle_period() {
        // Otherwise the lock command runs again every tick for as long as the
        // session stays idle.
        let mut idle = Idle::default();
        let s = settings(Some(300), Some(600));

        assert_eq!(idle.tick(&s, Duration::from_secs(299)), Actions::default());
        assert_eq!(
            idle.tick(&s, Duration::from_secs(300)),
            Actions {
                lock: true,
                blank: false
            }
        );
        assert_eq!(idle.tick(&s, Duration::from_secs(400)), Actions::default());
        assert_eq!(
            idle.tick(&s, Duration::from_secs(600)),
            Actions {
                lock: false,
                blank: true
            }
        );
        assert_eq!(idle.tick(&s, Duration::from_secs(900)), Actions::default());
    }

    #[test]
    fn activity_rearms_both_and_reports_a_blanked_screen() {
        let mut idle = Idle::default();
        let s = settings(Some(1), Some(2));
        idle.tick(&s, Duration::from_secs(5));
        assert!(idle.blanked());

        // The one thing activity has to undo rather than merely reset.
        assert!(
            idle.activity(Activity::Deliberate),
            "the screens were off and must come back"
        );
        assert!(!idle.blanked());
        assert!(!idle.activity(Activity::Deliberate), "already awake");

        // And both deadlines can fire again.
        assert_eq!(
            idle.tick(&s, Duration::from_secs(5)),
            Actions {
                lock: true,
                blank: true
            }
        );
    }

    #[test]
    fn an_inhibitor_holds_everything_off() {
        // And counts as activity, so releasing it starts the countdown again
        // rather than firing the instant it is released.
        let mut idle = Idle::default();
        let s = settings(Some(1), Some(1));
        idle.set_inhibited(true);
        assert_eq!(idle.tick(&s, Duration::from_secs(600)), Actions::default());

        idle.set_inhibited(false);
        assert_eq!(
            idle.tick(&s, Duration::from_secs(600)),
            Actions {
                lock: true,
                blank: true
            }
        );
    }

    #[test]
    fn a_forced_blank_is_undone_by_activity() {
        // Blanking on demand has to leave the same state the timer would, or
        // there is no way back short of a deadline that has already passed.
        let mut idle = Idle::default();
        idle.force_blank();
        assert!(idle.blanked());

        // The release of the chord first, which is what the grace is for, and
        // then a key somebody pressed at a dark screen.
        assert!(!idle.activity(Activity::Release));
        assert!(idle.activity(Activity::Deliberate));
        assert!(!idle.blanked());
    }

    #[test]
    fn a_press_at_a_dark_screen_wakes_it_at_once() {
        // The grace waits for the chord to come up, not for a stretch of wall
        // time: a fixed window means keys typed in the dark go into whatever
        // holds focus with nothing on screen to show for them.
        let mut idle = Idle::default();
        idle.force_blank();

        assert!(
            idle.activity(Activity::Deliberate),
            "a press had to wait out a clock"
        );
        assert!(!idle.blanked());
    }

    #[test]
    fn every_key_of_the_chord_may_come_up() {
        // A chord is three keys and three releases, and the second and third
        // are no more a wake than the first.
        let mut idle = Idle::default();
        idle.force_blank();

        assert!(!idle.activity(Activity::Release));
        assert!(!idle.activity(Activity::Release));
        assert!(idle.blanked());
    }

    #[test]
    fn a_release_at_a_deadline_blank_still_wakes() {
        // Nothing was pressed to get here, so there is no release in flight to
        // wait for and a hand coming off the keyboard is ordinary input.
        let mut idle = Idle::default();
        let s = settings(None, Some(1));
        idle.tick(&s, Duration::from_secs(5));
        assert!(idle.blanked());
        assert!(idle.activity(Activity::Release));
    }

    #[test]
    fn the_keystroke_that_blanked_does_not_immediately_unblank() {
        // `blank` is reachable from a chord, and a chord is at least a press
        // and a release. Without the grace the release is activity, activity
        // wakes the screens, and the binding looks like a dead key — which is
        // the whole reason the C build carries `BLANK_GRACE_USEC`.
        let mut idle = Idle::default();
        idle.force_blank();

        assert!(
            !idle.activity(Activity::Release),
            "the release woke the screens back up"
        );
        assert!(idle.blanked(), "and it cleared the flag on the way past");
    }

    #[test]
    fn a_deadline_blank_wakes_on_the_first_input() {
        // The asymmetry is the point: no keystroke asked for this one, so
        // there is no release in flight and nothing to wait out. Sharing the
        // grace here would mean a screen that stays black for half a second
        // after the mouse has already moved.
        let mut idle = Idle::default();
        let s = settings(None, Some(1));
        assert_eq!(
            idle.tick(&s, Duration::from_secs(5)),
            Actions {
                lock: false,
                blank: true
            }
        );
        assert!(
            idle.activity(Activity::Deliberate),
            "the deadline's blank made the user wait"
        );
        assert!(!idle.blanked());
    }
}
