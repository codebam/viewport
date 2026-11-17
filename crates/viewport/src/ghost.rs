// SPDX-License-Identifier: GPL-3.0-or-later
//
// windowsOut: the last frame of a closing window, faded out by the compositor.
//
// Every other window animation is the shell's — it sends `view.opacity` and
// `view.layout` a frame at a time and the compositor draws what it is told. A
// close cannot be: the client destroys its surface before the shell hears the
// window is gone (`toplevel_destroyed` unmaps and removes before it notifies
// `view.removed`), so there is nothing left for the page to animate. The
// compositor keeps the last frame it drew of the window instead, and fades
// that out itself. This module is the part that needs no renderer: the kept
// frame's description, the motion it fades under, and the arithmetic of the
// fade. Rendering it is `render::build`; taking the snapshot is the render
// pass, which is where a renderer lives.

use std::time::{Duration, Instant};

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::backend::renderer::element::Id;
use smithay::utils::{Logical, Physical, Point, Rectangle, Size, Transform};

/// How a closing window leaves. The entrance-only styles (`popin`, `gnomed`)
/// are not here: a window leaving fades, slides, zooms, or slides and fades.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutStyle {
    #[default]
    Fade,
    Slide,
    Slidefade,
    Zoom,
}

impl OutStyle {
    fn named(name: &str) -> Option<Self> {
        match name {
            "fade" => Some(Self::Fade),
            "slide" => Some(Self::Slide),
            "slidefade" => Some(Self::Slidefade),
            "zoom" => Some(Self::Zoom),
            _ => None,
        }
    }

    fn fades(self) -> bool {
        matches!(self, Self::Fade | Self::Slidefade | Self::Zoom)
    }

    fn slides(self) -> bool {
        matches!(self, Self::Slide | Self::Slidefade)
    }

    fn zooms(self) -> bool {
        matches!(self, Self::Zoom)
    }
}

/// The curve the fade runs under. A spring overrides the duration the way it
/// does for the shell's animations; the bezier and linear curves run over it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Curve {
    Linear,
    Bezier([f64; 4]),
    Spring {
        mass: f64,
        stiffness: f64,
        damping: f64,
    },
}

/// The motion one ghost fades under, resolved from the `windows_out` leaf and
/// its inheritance (`windows`, then the block's own pace and curve).
#[derive(Debug, Clone, Copy)]
pub struct GhostMotion {
    pub duration: Duration,
    pub style: OutStyle,
    pub curve: Curve,
}

impl Default for GhostMotion {
    fn default() -> Self {
        Self {
            duration: Duration::from_millis(200),
            style: OutStyle::Fade,
            curve: Curve::Linear,
        }
    }
}

/// Resolve the close motion from the validated config event, inheriting the
/// way the shell's tree does: a field the `windows_out` leaf does not name
/// comes from `windows`, then from the block's root. Absent everywhere is the
/// default — a fifth of a second, fading, linear.
pub fn resolve_motion(motion: Option<&viewport_ipc::event::Motion>) -> GhostMotion {
    let Some(motion) = motion else {
        return GhostMotion::default();
    };
    if motion.enabled == Some(false) {
        // Motion off is no animation at all: a zero duration expires the ghost
        // on its first sample, which is the instant removal it asks for.
        return GhostMotion {
            duration: Duration::ZERO,
            ..GhostMotion::default()
        };
    }
    let leaf = motion.windows_out.as_ref();
    let umbrella = motion.windows.as_ref();

    let style = leaf
        .and_then(|l| l.style.as_deref())
        .and_then(OutStyle::named)
        .or_else(|| {
            umbrella
                .and_then(|l| l.style.as_deref())
                .and_then(OutStyle::named)
        })
        .unwrap_or_default();

    let duration_ms = leaf
        .and_then(|l| l.duration)
        .or_else(|| umbrella.and_then(|l| l.duration))
        .or(motion.duration)
        .filter(|v| v.is_finite() && *v >= 0.0)
        .unwrap_or(200.0);

    // A spring named on the leaf (or the umbrella) wins over the curve, as it
    // does for the shell. Only a physical one counts; the validation upstream
    // has already dropped the rest, but a spring inherited from `windows`
    // arrives here as numbers and is checked again for the same reason.
    let spring = leaf
        .and_then(|l| l.spring)
        .or_else(|| umbrella.and_then(|l| l.spring));
    let curve = if let Some(s) = spring {
        if s.mass > 0.0 && s.stiffness > 0.0 && s.damping >= 0.0 {
            Curve::Spring {
                mass: s.mass,
                stiffness: s.stiffness,
                damping: s.damping,
            }
        } else {
            bezier_or_linear(leaf, umbrella, motion)
        }
    } else {
        bezier_or_linear(leaf, umbrella, motion)
    };

    GhostMotion {
        duration: Duration::from_secs_f64(duration_ms / 1000.0),
        style,
        curve,
    }
}

fn bezier_or_linear(
    leaf: Option<&viewport_ipc::event::MotionCategory>,
    umbrella: Option<&viewport_ipc::event::MotionCategory>,
    motion: &viewport_ipc::event::Motion,
) -> Curve {
    leaf.and_then(|l| l.bezier)
        .or_else(|| umbrella.and_then(|l| l.bezier))
        .or(motion.bezier)
        .map(Curve::Bezier)
        .unwrap_or(Curve::Linear)
}

/// Resolve a fade leaf (`fadeDim`, `fadePopups`) to its duration and curve.
/// These inherit from the block's root rather than from `windows`, and absent
/// is a fifth of a second linear. Motion off is a zero duration, which
/// `fade_progress` reads as "already there" — the snap it asks for.
pub fn resolve_fade(
    motion: Option<&viewport_ipc::event::Motion>,
    leaf: Option<&viewport_ipc::event::MotionCategory>,
) -> (Duration, Curve) {
    let Some(motion) = motion else {
        return (Duration::from_millis(200), Curve::Linear);
    };
    if motion.enabled == Some(false) {
        return (Duration::ZERO, Curve::Linear);
    }
    let duration_ms = leaf
        .and_then(|l| l.duration)
        .or(motion.duration)
        .filter(|v| v.is_finite() && *v >= 0.0)
        .unwrap_or(200.0);
    let bezier = leaf
        .and_then(|l| l.bezier)
        .or(motion.bezier)
        .map(Curve::Bezier)
        .unwrap_or(Curve::Linear);
    let curve = match leaf.and_then(|l| l.spring) {
        Some(s) if s.mass > 0.0 && s.stiffness > 0.0 && s.damping >= 0.0 => Curve::Spring {
            mass: s.mass,
            stiffness: s.stiffness,
            damping: s.damping,
        },
        _ => bezier,
    };
    (Duration::from_secs_f64(duration_ms / 1000.0), curve)
}

/// How far a fade has run, 0 at the start to 1 at the duration. A zero
/// duration is 1 at once — no animation, the value it would reach.
pub fn fade_progress(duration: Duration, curve: Curve, elapsed: Duration) -> f32 {
    if duration.is_zero() || elapsed >= duration {
        return 1.0;
    }
    let t = elapsed.as_secs_f64() / duration.as_secs_f64();
    let p = match curve {
        Curve::Linear => t,
        Curve::Bezier(b) => cubic_bezier(b, t),
        Curve::Spring {
            mass,
            stiffness,
            damping,
        } => spring_progress(mass, stiffness, damping, elapsed.as_secs_f64()),
    };
    p.clamp(0.0, 1.0) as f32
}

/// One closing window's kept frame, and where and how it fades.
pub struct Ghost {
    /// The window's own surface tree as the compositor last drew it, in
    /// physical pixels at scale 1 — the same picture `read_window_pixels`
    /// produces for a window share.
    pub buffer: MemoryRenderBuffer,
    /// Where the window's box sat, in global logical coordinates. The frame
    /// builder makes it output-relative and physical, the way it does a
    /// window's, so a ghost on a second monitor lands on that monitor.
    pub location: Point<f64, Logical>,
    /// The box's logical size: the buffer is drawn into it, so a window on a
    /// scaled output shrinks its physical pixels back to the size it showed.
    pub size: Size<i32, Logical>,
    /// The whole buffer, in its own logical coordinates (scale 1).
    pub src: Rectangle<f64, Logical>,
    /// Stable for the ghost's life, so the damage tracker follows one element
    /// rather than a fresh one each frame.
    pub id: Id,
    pub started: Instant,
    pub motion: GhostMotion,
    /// How far a slide carries the ghost, in logical pixels, signed toward the
    /// nearer side of the output it was on. Zero for a style that does not
    /// slide.
    pub slide_dx: f64,
}

/// What a ghost looks like `elapsed` into its fade. `None` once it is spent.
pub struct Sample {
    pub alpha: f32,
    /// Multiplier on the drawn size, about the ghost's own centre.
    pub scale: f64,
    /// Physical pixels to shift along x.
    pub dx: f64,
}

impl Ghost {
    pub fn new(
        pixels: &[u8],
        size: Size<i32, Physical>,
        location: Point<f64, Logical>,
        drawn: Size<i32, Logical>,
        motion: GhostMotion,
        slide_dx: f64,
    ) -> Self {
        let buffer = MemoryRenderBuffer::from_slice(
            pixels,
            Fourcc::Xrgb8888,
            (size.w, size.h),
            1,
            Transform::Normal,
            None,
        );
        Self {
            buffer,
            location,
            size: drawn,
            src: Rectangle::from_size((size.w as f64, size.h as f64).into()),
            id: Id::new(),
            started: Instant::now(),
            motion,
            slide_dx,
        }
    }

    /// The ghost at `elapsed`, or `None` once the fade is over. A zero
    /// duration (motion off) is spent immediately.
    pub fn sample(&self, elapsed: Duration) -> Option<Sample> {
        let duration = self.motion.duration;
        if duration.is_zero() || elapsed >= duration {
            return None;
        }
        let secs = elapsed.as_secs_f64();
        let p = match self.motion.curve {
            Curve::Linear => (secs / duration.as_secs_f64()).clamp(0.0, 1.0),
            Curve::Bezier(b) => cubic_bezier(b, secs / duration.as_secs_f64()),
            Curve::Spring {
                mass,
                stiffness,
                damping,
            } => spring_progress(mass, stiffness, damping, secs),
        };
        let p = p.clamp(0.0, 1.0);
        let alpha = if self.motion.style.fades() {
            (1.0 - p) as f32
        } else {
            1.0
        };
        let scale = if self.motion.style.zooms() {
            1.0 - 0.15 * p
        } else {
            1.0
        };
        let dx = if self.motion.style.slides() {
            self.slide_dx * p
        } else {
            0.0
        };
        Some(Sample { alpha, scale, dx })
    }
}

/// The y of a cubic bezier whose x is `x`, the CSS timing-function shape: the
/// curve is parametric, so the parameter is found by bisection on x and then y
/// is read at it. `x` is clamped to [0, 1]; the control points' y may overshoot
/// (an ease-back does), which is the point of letting them.
fn cubic_bezier([x1, y1, x2, y2]: [f64; 4], x: f64) -> f64 {
    let x = x.clamp(0.0, 1.0);
    let bez = |a: f64, b: f64, t: f64| {
        let u = 1.0 - t;
        3.0 * u * u * t * a + 3.0 * u * t * t * b + t * t * t
    };
    let mut lo = 0.0;
    let mut hi = 1.0;
    let mut t = x;
    for _ in 0..24 {
        let at = bez(x1, x2, t);
        if (at - x).abs() < 1e-5 {
            break;
        }
        if at < x {
            lo = t;
        } else {
            hi = t;
        }
        t = (lo + hi) / 2.0;
    }
    bez(y1, y2, t)
}

/// A damped spring released from rest at 0 toward 1, closed form. Under- and
/// critically-damped both converge without a numeric integrator; an
/// over-damped one (ζ > 1) is the slower sum of two decays. The shell's
/// reference integrator is the same physics, sampled per frame rather than
/// solved.
fn spring_progress(mass: f64, stiffness: f64, damping: f64, t: f64) -> f64 {
    let omega = (stiffness / mass).sqrt();
    let zeta = damping / (2.0 * (stiffness * mass).sqrt());
    if zeta < 1.0 {
        let wd = omega * (1.0 - zeta * zeta).sqrt();
        let decay = (-zeta * omega * t).exp();
        1.0 - decay * ((wd * t).cos() + (zeta * omega / wd) * (wd * t).sin())
    } else if (zeta - 1.0).abs() < 1e-9 {
        let decay = (-omega * t).exp();
        1.0 - decay * (1.0 + omega * t)
    } else {
        let root = (zeta * zeta - 1.0).sqrt();
        let r1 = -omega * (zeta - root);
        let r2 = -omega * (zeta + root);
        let a = r2 / (r2 - r1);
        let b = -r1 / (r2 - r1);
        1.0 - (a * (r1 * t).exp() + b * (r2 * t).exp())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_linear_fade_runs_from_opaque_to_nothing() {
        let motion = GhostMotion {
            duration: Duration::from_millis(200),
            style: OutStyle::Fade,
            curve: Curve::Linear,
        };
        let ghost = Ghost::new(
            &[0; 4],
            (1, 1).into(),
            (0.0, 0.0).into(),
            (1, 1).into(),
            motion,
            0.0,
        );
        // Sampled relative to its own start, so the test does not race clock.
        let at = |ms: u64| {
            let elapsed = Duration::from_millis(ms);
            ghost.sample(elapsed)
        };
        assert_eq!(at(0).unwrap().alpha, 1.0);
        assert!((at(100).unwrap().alpha - 0.5).abs() < 1e-6);
        assert!(at(200).is_none(), "the fade is spent at its duration");
        assert!(at(500).is_none());
    }

    #[test]
    fn motion_off_is_spent_at_once() {
        let motion = GhostMotion {
            duration: Duration::ZERO,
            ..GhostMotion::default()
        };
        let ghost = Ghost::new(
            &[0; 4],
            (1, 1).into(),
            (0.0, 0.0).into(),
            (1, 1).into(),
            motion,
            0.0,
        );
        assert!(ghost.sample(Duration::ZERO).is_none());
    }

    #[test]
    fn a_slide_keeps_its_opacity_and_moves() {
        let motion = GhostMotion {
            duration: Duration::from_millis(100),
            style: OutStyle::Slide,
            curve: Curve::Linear,
        };
        let ghost = Ghost::new(
            &[0; 4],
            (1, 1).into(),
            (0.0, 0.0).into(),
            (1, 1).into(),
            motion,
            1920.0,
        );
        let half = ghost.sample(Duration::from_millis(50)).unwrap();
        assert_eq!(half.alpha, 1.0, "a pure slide does not fade");
        assert!((half.dx - 960.0).abs() < 1e-6);
    }

    #[test]
    fn a_bezier_curve_eases_the_fade() {
        // ease-out: fast at the start, slow at the end — more than half faded
        // by the midpoint.
        let motion = GhostMotion {
            duration: Duration::from_millis(200),
            style: OutStyle::Fade,
            curve: Curve::Bezier([0.0, 0.0, 0.58, 1.0]),
        };
        let ghost = Ghost::new(
            &[0; 4],
            (1, 1).into(),
            (0.0, 0.0).into(),
            (1, 1).into(),
            motion,
            0.0,
        );
        let half = ghost.sample(Duration::from_millis(100)).unwrap();
        assert!(half.alpha < 0.5, "ease-out is past half at the midpoint");
    }

    #[test]
    fn a_critically_damped_spring_converges_without_overshoot() {
        // ζ = 1: c = 2√(km).
        let m = 1.0_f64;
        let k = 400.0_f64;
        let c = 2.0 * (k * m).sqrt();
        let mut prev = 0.0;
        for ms in (0..1000).step_by(10) {
            let p = spring_progress(m, k, c, ms as f64 / 1000.0);
            assert!(p >= prev - 1e-9, "critically damped never overshoots");
            assert!(p <= 1.0 + 1e-9);
            prev = p;
        }
        assert!(prev > 0.99, "and converges within a second");
    }

    #[test]
    fn resolution_inherits_the_windows_umbrella() {
        let motion = viewport_ipc::event::Motion {
            duration: Some(90.0),
            windows: Some(viewport_ipc::event::MotionCategory {
                duration: Some(120.0),
                ..Default::default()
            }),
            windows_out: Some(viewport_ipc::event::MotionCategory {
                style: Some("slidefade".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let resolved = resolve_motion(Some(&motion));
        assert_eq!(resolved.style, OutStyle::Slidefade);
        assert_eq!(resolved.duration, Duration::from_millis(120));
    }

    #[test]
    fn resolution_defaults_when_nothing_is_named() {
        let resolved = resolve_motion(None);
        assert_eq!(resolved.style, OutStyle::Fade);
        assert_eq!(resolved.duration, Duration::from_millis(200));
        assert_eq!(resolved.curve, Curve::Linear);
    }

    #[test]
    fn a_fade_leaf_inherits_the_root_not_windows() {
        let motion = viewport_ipc::event::Motion {
            duration: Some(300.0),
            windows: Some(viewport_ipc::event::MotionCategory {
                duration: Some(120.0),
                ..Default::default()
            }),
            fade_dim: Some(viewport_ipc::event::MotionCategory {
                duration: Some(150.0),
                ..Default::default()
            }),
            ..Default::default()
        };
        // The leaf's own duration wins.
        let (dur, _) = resolve_fade(Some(&motion), motion.fade_dim.as_ref());
        assert_eq!(dur, Duration::from_millis(150));
        // An absent leaf takes the root's, not the windows umbrella's.
        let (dur, _) = resolve_fade(Some(&motion), motion.fade_popups.as_ref());
        assert_eq!(dur, Duration::from_millis(300));
        // Nothing named is the default fifth of a second.
        let (dur, _) = resolve_fade(None, None);
        assert_eq!(dur, Duration::from_millis(200));
    }

    #[test]
    fn motion_off_is_a_zero_duration_fade() {
        let motion = viewport_ipc::event::Motion {
            enabled: Some(false),
            duration: Some(300.0),
            ..Default::default()
        };
        let (dur, _) = resolve_fade(Some(&motion), None);
        assert_eq!(dur, Duration::ZERO);
        // A zero duration is already there — the snap motion off asks for.
        assert_eq!(
            fade_progress(Duration::ZERO, Curve::Linear, Duration::ZERO),
            1.0
        );
    }

    #[test]
    fn fade_progress_runs_zero_to_one() {
        let d = Duration::from_millis(200);
        assert_eq!(fade_progress(d, Curve::Linear, Duration::ZERO), 0.0);
        assert!((fade_progress(d, Curve::Linear, Duration::from_millis(100)) - 0.5).abs() < 1e-6);
        assert_eq!(
            fade_progress(d, Curve::Linear, Duration::from_millis(200)),
            1.0
        );
        assert_eq!(
            fade_progress(d, Curve::Linear, Duration::from_millis(500)),
            1.0
        );
        // An ease-out curve is past half at the midpoint.
        let ease_out = Curve::Bezier([0.0, 0.0, 0.58, 1.0]);
        assert!(fade_progress(d, ease_out, Duration::from_millis(100)) > 0.5);
    }
}
