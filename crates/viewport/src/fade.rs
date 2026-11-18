// SPDX-License-Identifier: GPL-3.0-or-later
//
// fadeDim and fadePopups — the motion leaves that ease a value rather than
// snap it. Hyprland's fadeDim animates the dimming of an inactive window;
// fadePopups animates a popup arriving. Both are the compositor's to draw
// here: the dimming is an opacity multiplier it applies, and popups are
// compositor-drawn surfaces, so neither is a transition the shell can run on
// a live element the way it does windowsIn.
//
// The arithmetic is `crate::ghost`'s — the same curve and spring the close
// fade uses — over a from/to pair rather than a kept frame.

use std::time::{Duration, Instant};

use crate::ghost::{fade_progress, resolve_fade, Curve};
use crate::state::ViewportState;

/// One value easing from `from` to `to` since `start`.
#[derive(Debug, Clone, Copy)]
pub struct Fade {
    pub from: f32,
    pub to: f32,
    pub start: Instant,
}

impl Fade {
    fn value(self, now: Instant, duration: Duration, curve: Curve) -> f32 {
        let p = fade_progress(duration, curve, now.duration_since(self.start));
        self.from + (self.to - self.from) * p
    }
}

impl ViewportState {
    /// fadeDim: a window's opacity eased toward its target rather than snapped.
    /// The target is what `effective_opacity` composes — the dimming multiplier
    /// among it — so focus moving dims a window over the leaf's duration instead
    /// of in one frame. A target that has not changed since the last frame keeps
    /// its fade; a new one starts from wherever the current value is, so a
    /// window caught mid-fade reverses smoothly rather than jumping.
    pub(crate) fn animated_dim_opacity(&mut self, view_id: u32, target: f32) -> f32 {
        let motion = self.config.motion.as_ref();
        let (duration, curve) = resolve_fade(motion, motion.and_then(|m| m.fade_dim.as_ref()));
        if duration.is_zero() {
            self.dim_anims.remove(&view_id);
            return target;
        }
        let now = Instant::now();
        let (from, to, start) = match self.dim_anims.get(&view_id).copied() {
            // The target moved: fade on from the value reached so far.
            Some(a) if (a.to - target).abs() > f32::EPSILON => {
                (a.value(now, duration, curve), target, now)
            }
            Some(a) => (a.from, a.to, a.start),
            // First sight of the window: it appears at its target, no fade.
            None => (target, target, now),
        };
        if (to - from).abs() <= f32::EPSILON {
            self.dim_anims.remove(&view_id);
            return to;
        }
        self.dim_anims.insert(view_id, Fade { from, to, start });
        let elapsed = now.duration_since(start);
        if elapsed >= duration {
            self.dim_anims.remove(&view_id);
            return to;
        }
        self.needs_render = true;
        from + (to - from) * fade_progress(duration, curve, elapsed)
    }

    /// fadePopups: how opaque a popup first seen at `start` is now. A popup
    /// with no recorded arrival (fade off) is fully drawn. The arrival is
    /// recorded the first time the popup is seen drawn, and kept once the fade
    /// is spent so it does not fade in again; `prune_fades` forgets it when the
    /// popup is destroyed.
    pub(crate) fn popup_fade_alpha(&mut self, popup_id: u32) -> f32 {
        let motion = self.config.motion.as_ref();
        let (duration, curve) = resolve_fade(motion, motion.and_then(|m| m.fade_popups.as_ref()));
        if duration.is_zero() {
            self.popup_fades.remove(&popup_id);
            return 1.0;
        }
        let now = Instant::now();
        let start = *self.popup_fades.entry(popup_id).or_insert(now);
        let elapsed = now.duration_since(start);
        if elapsed >= duration {
            // Settled: fully drawn, and the entry stays. Removing it here made
            // the next frame record a fresh arrival and fade in again — a menu
            // blinking in and out on every fade period.
            return 1.0;
        }
        self.needs_render = true;
        fade_progress(duration, curve, elapsed)
    }

    /// Forget the fades of windows and popups that are gone, so the maps do
    /// not grow with every window ever opened. Called from the frame builder.
    pub(crate) fn prune_fades(&mut self, live_views: &[u32], live_popups: &[u32]) {
        self.dim_anims.retain(|id, _| live_views.contains(id));
        self.popup_fades.retain(|id, _| live_popups.contains(id));
    }
}
