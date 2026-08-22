// SPDX-License-Identifier: GPL-3.0-or-later
//
// The screen magnifier.
//
// A region of one real output, drawn larger and following the pointer. This is
// not `canvas.zoom`, which is a layout the shell draws at a scale and whose own
// header records that a click only reaches the pixel it appears to at 1.0 —
// that is the opposite of what a magnifier is for, because the entire point of
// magnifying a screen is to be able to use what is on it.
//
// So the whole design turns on one decision, and it is the decision that
// magnifiers get wrong:
//
// **The pointer's position is not magnified. The picture is.**
//
// The compositor's cursor lives at a real place in the layout, exactly where
// it would be with the magnifier off. What changes is where that place is
// *drawn*: the region under the cursor is blown up about the output's corner,
// and the cursor is drawn through the same transform as everything else, so it
// lands on top of the same content it was on top of before. Hit testing,
// focus-follows-mouse, drags, resize edges, the shell's own `:hover` — none of
// them are told the magnifier exists, and none of them can therefore disagree
// with it. A magnifier that instead moved the pointer would have to correct
// every one of those, and the first one anybody forgot would be a click
// landing a few hundred pixels from where it was aimed.
//
// The exception is input that arrives as a *place on the glass* rather than as
// a movement: a touchscreen, and a tablet in absolute mode. Those name a
// physical spot on the panel, and what is at that spot is magnified content —
// so those, and only those, are mapped back through [`View::to_content`]
// before anything else sees them. That asymmetry is the whole of the input
// half, and it is why `to_content` and `to_screen` are inverses that are
// tested as such rather than two separate pieces of arithmetic.
//
// A capture — a screenshot, a screen share, a recording — comes out magnified,
// because every one of them composites the same element list this transform is
// applied to. That is the right answer rather than a leak: a capture of the
// screen is a capture of what is on it, and a share where the person talking
// is looking at 4x and everybody else is looking at 1x is a share where
// nothing anybody points at is where they say it is.
//
// One more consequence worth stating, because it looks like an oversight: only
// the output the pointer is on is magnified. Magnifying all of them would mean
// choosing a region for a screen the pointer is nowhere near, and every answer
// to that is wrong in a different way — clamped to a corner it drifts to the
// edge and stays there, centred it shows the middle of a screen nobody is
// looking at. The pointer is the thing being followed, and it is on one
// screen at a time.

use smithay::utils::{Logical, Physical, Point, Rectangle};

/// The largest zoom that can be configured, whatever `magnify.max` says.
///
/// Not a taste limit. Every element on the output is scaled about a point, and
/// at a large enough factor the scaled geometry of a full-screen surface
/// overflows the `i32` physical rectangles the damage tracker works in — which
/// does not fail loudly, it wraps, and the screen fills with garbage. Thirty-
/// two is far past any useful magnification (a 24pt line becomes eight inches
/// tall) and far short of the point where a 4K surface's width stops fitting.
pub const ZOOM_CEILING: f64 = 32.0;

/// The smallest step that is worth having.
///
/// A step below this rounds to no visible change on one press, so the chord
/// looks dead; a configuration file that asks for one is more likely to be a
/// typo than a preference.
pub const MIN_STEP: f64 = 0.05;

/// The default step and maximum, when the config file says nothing.
///
/// 0.5 and 8.0 are what Orca and GNOME's own magnifier settle around: a step
/// small enough that the second press is still readable and large enough that
/// getting to a useful size is two or three presses rather than a dozen.
pub const DEFAULT_STEP: f64 = 0.5;
pub const DEFAULT_MAX: f64 = 8.0;

/// How far off 1.0 still counts as off.
///
/// Floating-point steps do not land back on exactly 1.0 — 1.0 + 0.3 - 0.3 is
/// 0.9999999999999998 — and a magnifier that is "on" at 0.9999999999999998
/// costs a full-screen rescale of every element, every frame, for a picture
/// nobody can tell from the unmagnified one.
const OFF: f64 = 1e-6;

/// The magnifier's setting for the session.
///
/// One zoom, not one per output: it is a property of the person looking at the
/// screen rather than of any screen, and a desk where the left monitor is
/// magnified and the right one is not is a desk where half the chords appear
/// not to work.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Magnifier {
    /// 1.0 is off. Never below 1.0: a magnifier that could shrink the screen
    /// is a different feature with the same key, and one that nobody would
    /// find by pressing zoom-out one time too many.
    zoom: f64,
    /// What one press of zoom-in adds, from `magnify.step`.
    step: f64,
    /// The largest zoom the chords will reach, from `magnify.max`.
    max: f64,
}

impl Default for Magnifier {
    fn default() -> Self {
        Self {
            zoom: 1.0,
            step: DEFAULT_STEP,
            max: DEFAULT_MAX,
        }
    }
}

impl Magnifier {
    /// Apply `magnify.step` and `magnify.max` from a config file.
    ///
    /// Returns whether the zoom in force changed, which it does when a reload
    /// lowers the maximum below where the screen currently is: the setting is
    /// a bound and not a suggestion, so the picture comes back down to meet
    /// it rather than staying somewhere the config file says is not allowed.
    ///
    /// Nonsense is clamped rather than refused. A `max` below 1.0 would be a
    /// magnifier that cannot magnify, and a negative `step` would be a
    /// zoom-in chord that zooms out; both are configuration mistakes, and the
    /// useful response to one is a working compositor with a sane setting
    /// rather than a refusal to start.
    pub fn configure(&mut self, step: Option<f64>, max: Option<f64>) -> bool {
        self.step = step
            .filter(|s| s.is_finite())
            .map_or(DEFAULT_STEP, |s| s.max(MIN_STEP));
        self.max = max
            .filter(|m| m.is_finite())
            .map_or(DEFAULT_MAX, |m| m.clamp(1.0, ZOOM_CEILING));
        let clamped = self.zoom.min(self.max);
        let changed = (clamped - self.zoom).abs() > f64::EPSILON;
        self.zoom = clamped;
        changed
    }

    /// The zoom in force. 1.0 when the magnifier is off.
    pub fn zoom(&self) -> f64 {
        self.zoom
    }

    pub fn is_on(&self) -> bool {
        self.zoom > 1.0 + OFF
    }

    /// One press of zoom-in, zoom-out or off. Returns whether anything moved,
    /// so the caller only repaints when there is something new to see.
    pub fn apply(&mut self, step: Step) -> bool {
        let was = self.zoom;
        self.zoom = match step {
            Step::In => (self.zoom + self.step).min(self.max),
            // Snapped to exactly 1.0 rather than left a step above it, because
            // the last press of zoom-out is the one that has to turn the
            // feature off — and `1.0 + step - step` is not 1.0. Without this
            // the magnifier stays on forever at a zoom nobody can see, paying
            // for a rescale of every element on every frame.
            Step::Out => {
                let next = self.zoom - self.step;
                if next <= 1.0 + self.step / 2.0 {
                    1.0
                } else {
                    next
                }
            }
            Step::Off => 1.0,
        };
        (self.zoom - was).abs() > f64::EPSILON
    }
}

/// What a magnifier chord does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    In,
    Out,
    /// Straight back to 1.0. Worth a chord of its own for the reason
    /// `canvas.home` is: at 8x, finding the zoom-out key by looking at the
    /// screen means finding it through the part of the screen that is on it.
    Off,
}

/// What one output shows while the magnifier is on.
///
/// Everything here is in the layout's own logical coordinates — the same
/// coordinates the pointer, the windows and the hit test are in — because the
/// whole property being preserved is that those are unchanged. Physical
/// pixels appear once, in [`View::origin_physical`], which is the only place
/// the renderer needs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct View {
    /// The output being magnified, in layout coordinates.
    pub output: Rectangle<i32, Logical>,
    pub zoom: f64,
    /// The top-left of the region the output shows, in layout coordinates.
    ///
    /// Always inside the output: the region is clamped so that magnifying near
    /// an edge slides it along rather than running off, which is what stops
    /// the magnifier from showing a strip of the neighbouring monitor along
    /// one side and nothing at all along the other.
    pub origin: Point<f64, Logical>,
}

impl View {
    /// The region this output shows with the pointer where it is.
    ///
    /// Centred on the pointer and clamped to the output. Clamped and not
    /// scrolled-at-the-edges (the "focus tracking" a lot of magnifiers do)
    /// because the pointer here is the compositor's real cursor: it cannot
    /// leave the screen, so a region that stayed centred on it would need to
    /// show pixels that are not on any monitor, and the honest answer at an
    /// edge is that there is nothing further that way.
    pub fn new(output: Rectangle<i32, Logical>, zoom: f64, pointer: Point<f64, Logical>) -> Self {
        let zoom = zoom.max(1.0);
        let width = f64::from(output.size.w) / zoom;
        let height = f64::from(output.size.h) / zoom;
        let left = f64::from(output.loc.x);
        let top = f64::from(output.loc.y);
        // `max(left)` after the min, so an output narrower than the region —
        // which cannot happen for zoom >= 1 but can for a zero-sized output
        // during a mode set — clamps to the corner instead of producing a
        // negative range and an origin outside the screen.
        let x = (pointer.x - width / 2.0)
            .min(left + f64::from(output.size.w) - width)
            .max(left);
        let y = (pointer.y - height / 2.0)
            .min(top + f64::from(output.size.h) - height)
            .max(top);
        Self {
            output,
            zoom,
            origin: (x, y).into(),
        }
    }

    /// Where a point in the layout is drawn on the screen.
    ///
    /// The forward transform, and the one the renderer performs on every
    /// element. Nothing calls it in the input path — that is the point — but
    /// it is what `to_content` is the inverse of, and the tests below check
    /// the pair rather than either alone.
    pub fn to_screen(self, at: Point<f64, Logical>) -> Point<f64, Logical> {
        let x = f64::from(self.output.loc.x) + (at.x - self.origin.x) * self.zoom;
        let y = f64::from(self.output.loc.y) + (at.y - self.origin.y) * self.zoom;
        (x, y).into()
    }

    /// What is under a physical spot on the glass.
    ///
    /// The inverse, and the only transform the input path performs. It is for
    /// a touchscreen and for a tablet in absolute mode, which name a place on
    /// the panel; a mouse names a movement, and a movement of the cursor is
    /// unaffected by what the screen is doing with the picture.
    pub fn to_content(self, at: Point<f64, Logical>) -> Point<f64, Logical> {
        let x = self.origin.x + (at.x - f64::from(self.output.loc.x)) / self.zoom;
        let y = self.origin.y + (at.y - f64::from(self.output.loc.y)) / self.zoom;
        (x, y).into()
    }

    /// The origin as the renderer needs it: output-local, in physical pixels.
    ///
    /// Output-local because every element in a [`crate::render::Frame`] is
    /// already positioned relative to its output's corner, and physical
    /// because that is the space the damage tracker composites in. The
    /// output's `transform` does not appear here and must not: a rotation is
    /// applied by the damage tracker to the finished element list, after these
    /// positions, so applying it here as well would rotate the magnified
    /// region twice.
    pub fn origin_physical(&self, scale: f64) -> Point<i32, Physical> {
        let local = Point::<f64, Logical>::from((
            self.origin.x - f64::from(self.output.loc.x),
            self.origin.y - f64::from(self.output.loc.y),
        ));
        local.to_physical(scale).to_i32_round()
    }

    /// How far the magnified picture has to be pushed back after being scaled
    /// about the output's corner, in output-local physical pixels.
    ///
    /// Smithay's `RescaleRenderElement` scales about a point and leaves that
    /// point fixed, which is not the transform wanted here: the region's
    /// origin must end up at the *corner of the output*, not where it started.
    /// Scaling about the corner and then translating by this is that
    /// transform, and it is done in two elements rather than by choosing a
    /// clever origin because the origin that would do it in one is
    /// `origin * zoom / (zoom - 1)`, which is a division by zero at the exact
    /// moment the magnifier is switched off.
    pub fn offset_physical(&self, scale: f64) -> Point<i32, Physical> {
        let origin = self.origin_physical(scale);
        Point::from((
            -(f64::from(origin.x) * self.zoom).round() as i32,
            -(f64::from(origin.y) * self.zoom).round() as i32,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output() -> Rectangle<i32, Logical> {
        Rectangle::new((0, 0).into(), (1920, 1080).into())
    }

    /// The property the whole feature rests on: a click lands where it looks
    /// like it lands.
    ///
    /// Under magnification the cursor is drawn at `to_screen(pointer)`. What
    /// is at that spot on the glass is `to_content` of it — and if that is not
    /// the pointer again, to the last fraction of a pixel, then a touch on the
    /// drawn cursor hits something the cursor is not on. The two transforms
    /// are each other's inverse or the feature is a lie.
    #[test]
    fn the_cursor_is_over_what_it_appears_to_be_over() {
        for zoom in [1.0, 1.5, 2.5, 4.0, 8.0, 31.999] {
            for pointer in [
                Point::<f64, Logical>::from((0.0, 0.0)),
                (960.0, 540.0).into(),
                (1919.0, 1079.0).into(),
                (37.5, 902.25).into(),
            ] {
                let view = View::new(output(), zoom, pointer);
                let drawn = view.to_screen(pointer);
                let back = view.to_content(drawn);
                assert!(
                    (back.x - pointer.x).abs() < 1e-9 && (back.y - pointer.y).abs() < 1e-9,
                    "zoom {zoom} pointer {pointer:?} came back as {back:?}"
                );
            }
        }
    }

    /// And the same for a point that is not the pointer: a window's corner, a
    /// button, the edge of a text field. Magnifying the screen must not change
    /// which surface-local pixel a given layout position is.
    #[test]
    fn a_surface_local_position_is_the_same_at_every_zoom() {
        // A window at an awkward place, and a point inside it.
        let window = Point::<f64, Logical>::from((613.0, 271.0));
        let inside = Point::<f64, Logical>::from((742.5, 480.0));
        let expected = (inside.x - window.x, inside.y - window.y);

        let pointer = inside;
        for zoom in [1.0, 2.5, 7.0] {
            let view = View::new(output(), zoom, pointer);
            // The compositor hit-tests the pointer's own position, which the
            // magnifier never touches, so the surface-local coordinate it
            // produces is arithmetic on numbers this module did not change.
            let local = (pointer.x - window.x, pointer.y - window.y);
            assert_eq!(local, expected, "zoom {zoom} moved a surface-local point");
            // And the touch path, which does go through the transform, must
            // arrive at the same place when it is aimed at the drawn cursor.
            let touched = view.to_content(view.to_screen(pointer));
            let touched_local = (touched.x - window.x, touched.y - window.y);
            assert!(
                (touched_local.0 - expected.0).abs() < 1e-9
                    && (touched_local.1 - expected.1).abs() < 1e-9,
                "zoom {zoom}: a touch on the cursor came out at {touched_local:?}"
            );
        }
    }

    /// An output that is not at the origin, is scaled, and is rotated.
    ///
    /// The second monitor of a two-monitor desk, at 1.5x, on its side. None of
    /// those may leak into the logical transform: the scale belongs to the
    /// renderer and the rotation is applied by the damage tracker after these
    /// positions, so a magnifier that consulted either here would apply it
    /// twice.
    #[test]
    fn a_scaled_rotated_output_off_the_origin_maps_the_same_way() {
        // Rotated, so the logical size is the panel's the other way round —
        // which the compositor has already resolved by the time this sees the
        // geometry, and is exactly why nothing here asks about the transform.
        let output = Rectangle::<i32, Logical>::new((2560, -240).into(), (1080, 1920).into());
        let pointer = Point::<f64, Logical>::from((2900.0, 700.0));
        let view = View::new(output, 2.5, pointer);

        let back = view.to_content(view.to_screen(pointer));
        assert!((back.x - pointer.x).abs() < 1e-9 && (back.y - pointer.y).abs() < 1e-9);

        // The region is inside the output it belongs to, and nowhere near the
        // one at the origin.
        assert!(view.origin.x >= f64::from(output.loc.x));
        assert!(view.origin.y >= f64::from(output.loc.y));
        assert!(view.origin.x + f64::from(output.size.w) / 2.5 <= f64::from(output.loc.x + 1080));

        // The physical origin is output-local: the layout offset is gone, and
        // only the scale is left.
        let physical = view.origin_physical(1.5);
        let expected_x = ((view.origin.x - f64::from(output.loc.x)) * 1.5).round() as i32;
        let expected_y = ((view.origin.y - f64::from(output.loc.y)) * 1.5).round() as i32;
        assert_eq!(physical, Point::from((expected_x, expected_y)));
    }

    /// The region stays on the screen at every zoom and from every corner.
    ///
    /// A magnifier that let the region run off the edge shows a band of
    /// whatever is laid out next to this monitor down one side, which on a
    /// single-monitor desk is nothing at all — a black stripe that moves when
    /// the pointer does.
    #[test]
    fn the_region_never_leaves_the_output() {
        let out = output();
        for zoom in [1.25, 2.0, 6.0] {
            for pointer in [
                Point::<f64, Logical>::from((0.0, 0.0)),
                (1920.0, 1080.0).into(),
                (-500.0, 3000.0).into(),
                (960.0, 540.0).into(),
            ] {
                let view = View::new(out, zoom, pointer);
                let w = f64::from(out.size.w) / zoom;
                let h = f64::from(out.size.h) / zoom;
                assert!(view.origin.x >= f64::from(out.loc.x) - 1e-9, "zoom {zoom}");
                assert!(view.origin.y >= f64::from(out.loc.y) - 1e-9, "zoom {zoom}");
                assert!(view.origin.x + w <= f64::from(out.loc.x + out.size.w) + 1e-9);
                assert!(view.origin.y + h <= f64::from(out.loc.y + out.size.h) + 1e-9);
            }
        }
    }

    /// Scaling about the output's corner and then translating puts the
    /// region's own origin exactly at that corner, which is what makes the
    /// magnified picture start at the top-left of the screen rather than
    /// wherever the region happened to be.
    #[test]
    fn the_region_is_drawn_from_the_corner_of_the_output() {
        let view = View::new(output(), 2.5, (1200.0, 300.0).into());
        let scale = 2.0;
        let origin = view.origin_physical(scale);
        let offset = view.offset_physical(scale);
        // What the two render elements do to the region's own top-left: scale
        // about (0, 0), then translate.
        let drawn_x = (f64::from(origin.x) * view.zoom).round() as i32 + offset.x;
        let drawn_y = (f64::from(origin.y) * view.zoom).round() as i32 + offset.y;
        assert_eq!((drawn_x, drawn_y), (0, 0));
    }

    /// Zoom-out has to reach exactly 1.0, or the magnifier never switches off
    /// and every frame pays for a rescale of the whole screen.
    #[test]
    fn zooming_out_lands_on_off() {
        let mut m = Magnifier::default();
        m.configure(Some(0.3), Some(8.0));
        for _ in 0..7 {
            m.apply(Step::In);
        }
        assert!(m.is_on());
        for _ in 0..7 {
            m.apply(Step::Out);
        }
        assert_eq!(m.zoom(), 1.0);
        assert!(!m.is_on());
        // And it does not go below.
        assert!(
            !m.apply(Step::Out),
            "zooming out past off changed something"
        );
        assert_eq!(m.zoom(), 1.0);
    }

    /// The maximum is a bound, not a suggestion: pressing zoom-in past it does
    /// nothing rather than creeping past.
    #[test]
    fn the_maximum_holds_and_a_reload_can_lower_it() {
        let mut m = Magnifier::default();
        m.configure(Some(1.0), Some(3.0));
        for _ in 0..10 {
            m.apply(Step::In);
        }
        assert_eq!(m.zoom(), 3.0);
        assert!(!m.apply(Step::In));

        // A reload that lowers the maximum brings the picture down with it,
        // and says so, because the screen has to be repainted.
        assert!(m.configure(Some(1.0), Some(2.0)));
        assert_eq!(m.zoom(), 2.0);
        assert!(!m.configure(Some(1.0), Some(2.0)));
    }

    /// Nonsense in the config file is clamped rather than obeyed. A negative
    /// step is a zoom-in chord that zooms out, and a maximum below 1.0 is a
    /// magnifier that cannot magnify.
    #[test]
    fn a_broken_config_still_gives_a_working_magnifier() {
        let mut m = Magnifier::default();
        m.configure(Some(-2.0), Some(0.25));
        assert!(
            !m.apply(Step::In),
            "a maximum clamped to 1.0 is a magnifier that stays off"
        );
        assert_eq!(m.zoom(), 1.0);

        m.configure(Some(f64::NAN), Some(f64::INFINITY));
        assert!(m.apply(Step::In));
        assert_eq!(m.zoom(), 1.0 + DEFAULT_STEP);

        m.configure(Some(1000.0), Some(1000.0));
        m.apply(Step::In);
        assert!(m.zoom() <= ZOOM_CEILING);
    }
}
