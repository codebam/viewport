// SPDX-License-Identifier: MIT
//
// Turning rectangles and transforms into the two affine maps the vertex shader
// needs.
//
// The shader draws a unit quad and knows nothing else: one map takes a corner
// to clip space, the other takes the same corner to a texture coordinate.
// Everything about rotation, flipping, source rectangles and output size is
// resolved here, in Rust, where it can be tested without a GPU.
//
// The maps are derived by evaluating the transform at three corners rather
// than by hand-writing eight cases. Every step in the chain is affine, so
// three points determine it — and deriving it from
// `Transform::transform_point_in` means this cannot drift from Smithay's own
// convention, which is the thing that would otherwise silently disagree.

use smithay::utils::{Physical, Point, Rectangle, Size, Transform};

/// An affine map from the unit square to somewhere, as the shader wants it.
///
/// `a` holds the two basis vectors and `b` the origin, so a point is
/// `a.x * u + a.y * v + b`. Packed as two `vec4`s because a `mat3x2` in a push
/// constant block has alignment rules that are easy to get subtly wrong.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Affine {
    /// `[e00, e01, e10, e11]` — the images of (1,0) and (0,1), relative to the
    /// origin.
    pub a: [f32; 4],
    /// `[e20, e21, 0, 0]` — the image of (0,0).
    pub b: [f32; 4],
}

impl Affine {
    /// Build from the images of the three corners that determine it.
    fn from_corners(origin: (f32, f32), right: (f32, f32), down: (f32, f32)) -> Self {
        Self {
            a: [
                right.0 - origin.0,
                right.1 - origin.1,
                down.0 - origin.0,
                down.1 - origin.1,
            ],
            b: [origin.0, origin.1, 0.0, 0.0],
        }
    }

    /// Apply to a unit-square coordinate. Only used by the tests; the shader
    /// does this itself.
    pub fn apply(&self, u: f32, v: f32) -> (f32, f32) {
        (
            self.a[0] * u + self.a[2] * v + self.b[0],
            self.a[1] * u + self.a[3] * v + self.b[1],
        )
    }
}

/// The map from a unit quad to clip space, for a rectangle in output
/// coordinates.
///
/// `output_size` is the **framebuffer**: the size Smithay's `Renderer::render`
/// is given, which is what the GLES renderer sets its viewport to before it
/// touches the transform at all (`gles/mod.rs`, `Viewport(0, 0, output_size)`).
///
/// `dst` is in the space *after* the transform — the one `Frame::output_size`
/// reports, and the one Smithay's damage tracker lays elements out in
/// (`output_geo = transform.transform_size(output_size)`). The two differ
/// whenever the transform swaps axes: a 2560x1440 panel rotated 90 degrees
/// scans out a 2560x1440 framebuffer holding a 1440x2560 desktop.
pub fn position(
    dst: Rectangle<i32, Physical>,
    output_size: Size<i32, Physical>,
    transform: Transform,
) -> Affine {
    let to_clip = |u: f32, v: f32| -> (f32, f32) {
        // Corner of the destination rectangle, in the transformed space.
        let x = dst.loc.x as f32 + u * dst.size.w as f32;
        let y = dst.loc.y as f32 + v * dst.size.h as f32;
        clip(x, y, output_size, transform)
    };

    Affine::from_corners(to_clip(0.0, 0.0), to_clip(1.0, 0.0), to_clip(0.0, 1.0))
}

/// A point in the transformed space, in clip coordinates.
///
/// The chain is Smithay's, taken from `GlesRenderer::render`: an orthographic
/// projection of the transformed space into OpenGL's -1..1 with +Y up,
/// `Transform::matrix()` applied there, and then the flip that puts +Y back
/// down — which is where Vulkan starts, so nothing more is needed here.
///
/// `Transform::matrix()` rather than `Transform::transform_point_in`, which
/// looks like it says the same thing and does not: for `Flipped90` and
/// `Flipped270` the two disagree by a half turn. `transform_point_in` maps
/// `Flipped90` to a bare transpose `(y, x)`, while the matrix is a transpose
/// *and* a 180-degree rotation. Every other renderer goes through the matrix,
/// so it is the one that decides where a pixel belongs — and a display set to
/// `flipped-90` came up upside down until this followed it.
fn clip(x: f32, y: f32, output_size: Size<i32, Physical>, transform: Transform) -> (f32, f32) {
    let area = transform.transform_size(output_size);

    // Into OpenGL's clip space, +Y up.
    let a = 2.0 * x / area.w as f32 - 1.0;
    let b = 1.0 - 2.0 * y / area.h as f32;

    // [e00, e01, e10, e11, e20, e21] — two basis vectors and a translation.
    let m = transform.matrix().to_cols_array();
    let tx = m[0] * a + m[2] * b + m[4];
    let ty = m[1] * a + m[3] * b + m[5];

    (tx, -ty)
}

/// Where a rectangle in the transformed space lands in the framebuffer.
///
/// The scissor is in framebuffer pixels, and everything else here is in the
/// space the transform produced, so this is the one place that has to go back
/// the other way. Through [`clip`], so a scissor cannot end up describing a
/// different rectangle than the quad it is clipping.
pub fn framebuffer_rect(
    rect: Rectangle<i32, Physical>,
    output_size: Size<i32, Physical>,
    transform: Transform,
) -> Rectangle<i32, Physical> {
    let corner = |x: i32, y: i32| -> (f32, f32) {
        let (cx, cy) = clip(x as f32, y as f32, output_size, transform);
        (
            (cx + 1.0) * 0.5 * output_size.w as f32,
            (cy + 1.0) * 0.5 * output_size.h as f32,
        )
    };

    // Two opposite corners are enough: every transform is axis-aligned, so the
    // image of a rectangle is a rectangle.
    let (x0, y0) = corner(rect.loc.x, rect.loc.y);
    let (x1, y1) = corner(rect.loc.x + rect.size.w, rect.loc.y + rect.size.h);

    let loc = Point::<i32, Physical>::from((x0.min(x1).round() as i32, y0.min(y1).round() as i32));
    let size = Size::<i32, Physical>::from((
        (x1 - x0).abs().round() as i32,
        (y1 - y0).abs().round() as i32,
    ));
    Rectangle::new(loc, size)
}

/// The map from a unit quad to normalised texture coordinates.
///
/// `src` is in buffer pixels. `transform` is the surface's buffer transform,
/// and is applied inverted — the trait describes rendering "after applying the
/// inverse of the given transformation", because the transform says how the
/// buffer is oriented relative to the surface and this goes the other way.
///
/// `flipped` is the separate, shm-specific notion of a buffer whose rows run
/// bottom-up.
pub fn texture(
    src: Rectangle<f64, smithay::utils::Buffer>,
    texture_size: (f64, f64),
    transform: Transform,
    flipped: bool,
) -> Affine {
    let inverse = transform.invert();
    // The region's extent as the surface sees it, which is the source space
    // the corner lives in before the transform is applied.
    let extent = inverse.transform_size(Size::<f64, smithay::utils::Buffer>::from((
        src.size.w, src.size.h,
    )));

    let (tw, th) = texture_size;

    let to_uv = |u: f64, v: f64| -> (f32, f32) {
        let v = if flipped { 1.0 - v } else { v };
        let point = inverse.transform_point_in(
            Point::<f64, smithay::utils::Buffer>::from((u * extent.w, v * extent.h)),
            &extent,
        );
        (
            ((src.loc.x + point.x) / tw) as f32,
            ((src.loc.y + point.y) / th) as f32,
        )
    };

    Affine::from_corners(to_uv(0.0, 0.0), to_uv(1.0, 0.0), to_uv(0.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: (f32, f32), b: (f32, f32)) -> bool {
        (a.0 - b.0).abs() < 1e-4 && (a.1 - b.1).abs() < 1e-4
    }

    #[test]
    fn an_untransformed_rect_maps_straight_to_clip_space() {
        // The whole of a 100x50 output.
        let size = Size::from((100, 50));
        let affine = position(
            Rectangle::new(Point::from((0, 0)), size),
            size,
            Transform::Normal,
        );
        assert!(close(affine.apply(0.0, 0.0), (-1.0, -1.0)), "top-left");
        assert!(close(affine.apply(1.0, 0.0), (1.0, -1.0)), "top-right");
        assert!(close(affine.apply(0.0, 1.0), (-1.0, 1.0)), "bottom-left");
        assert!(close(affine.apply(1.0, 1.0), (1.0, 1.0)), "bottom-right");
    }

    #[test]
    fn a_quadrant_lands_in_its_quadrant() {
        // Top-left quarter of a 100x100 output is the top-left quarter of clip
        // space, which runs -1..1.
        let size = Size::from((100, 100));
        let affine = position(
            Rectangle::new(Point::from((0, 0)), Size::from((50, 50))),
            size,
            Transform::Normal,
        );
        assert!(close(affine.apply(0.0, 0.0), (-1.0, -1.0)));
        assert!(close(affine.apply(1.0, 1.0), (0.0, 0.0)));
    }

    #[test]
    fn a_rotated_output_moves_the_corner_round() {
        // A 100x50 framebuffer on a display rotated 90 degrees, so the desktop
        // drawn into it is 50x100. transform_point_in maps (0,0) in that 50x100
        // area to (50, 0) — the top-right corner of the framebuffer.
        let framebuffer = Size::from((100, 50));
        let transform = Transform::_90;
        let affine = position(
            Rectangle::from_size(transform.transform_size(framebuffer)),
            framebuffer,
            transform,
        );
        assert!(close(affine.apply(0.0, 0.0), (1.0, -1.0)), "top-left goes right");
        // And the whole quad still covers the whole framebuffer.
        for (u, v) in [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)] {
            let (x, y) = affine.apply(u, v);
            assert!((-1.0..=1.0).contains(&x) && (-1.0..=1.0).contains(&y));
        }
    }

    #[test]
    fn every_output_transform_covers_the_framebuffer_exactly_once() {
        // Whatever the rotation, the four corners of a full-output quad have
        // to be the four corners of clip space — no gaps, no overlap.
        let size = Size::<i32, Physical>::from((80, 40));
        for transform in [
            Transform::Normal,
            Transform::_90,
            Transform::_180,
            Transform::_270,
            Transform::Flipped,
            Transform::Flipped90,
            Transform::Flipped180,
            Transform::Flipped270,
        ] {
            let affine = position(
                Rectangle::from_size(transform.transform_size(size)),
                size,
                transform,
            );
            let mut corners: Vec<(i32, i32)> = [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)]
                .into_iter()
                .map(|(u, v)| {
                    let (x, y) = affine.apply(u, v);
                    (x.round() as i32, y.round() as i32)
                })
                .collect();
            corners.sort();
            assert_eq!(
                corners,
                vec![(-1, -1), (-1, 1), (1, -1), (1, 1)],
                "{transform:?} does not cover clip space"
            );
        }
    }

    /// A flip is not a rotation, and the difference is visible.
    ///
    /// `Flipped90` is `_90` mirrored, so a quadrant that lands in one half of
    /// the framebuffer under one has to land in the other half under the other.
    /// Deriving the map from `transform_point_in` put them in the same place —
    /// the transpose it uses for `Flipped90` is the matrix's transpose turned
    /// half a turn — and a monitor set to `flipped-90` came up upside down.
    #[test]
    fn a_flip_is_not_the_rotation_it_shares_a_name_with() {
        // A 32x16 framebuffer holds a 16x32 desktop under either transform.
        let framebuffer = Size::<i32, Physical>::from((32, 16));
        let quadrant = Rectangle::new(Point::from((0, 0)), Size::from((8, 8)));

        let rotated = framebuffer_rect(quadrant, framebuffer, Transform::_90);
        let flipped = framebuffer_rect(quadrant, framebuffer, Transform::Flipped90);

        assert_eq!(
            rotated,
            Rectangle::new(Point::from((24, 0)), Size::from((8, 8))),
            "_90 puts the desktop's top-left in the framebuffer's top-right"
        );
        assert_eq!(
            flipped,
            Rectangle::new(Point::from((24, 8)), Size::from((8, 8))),
            "Flipped90 mirrors that into the bottom-right"
        );
    }

    /// The scissor and the quad describe the same rectangle.
    #[test]
    fn a_scissor_covers_exactly_what_the_quad_covers() {
        let framebuffer = Size::<i32, Physical>::from((80, 40));
        for transform in [
            Transform::Normal,
            Transform::_90,
            Transform::_180,
            Transform::_270,
            Transform::Flipped,
            Transform::Flipped90,
            Transform::Flipped180,
            Transform::Flipped270,
        ] {
            let area = transform.transform_size(framebuffer);
            let rect = Rectangle::new(
                Point::from((area.w / 4, area.h / 8)),
                Size::from((area.w / 2, area.h / 4)),
            );
            let scissor = framebuffer_rect(rect, framebuffer, transform);
            let affine = position(rect, framebuffer, transform);

            // The quad's corners, back out of clip space into pixels.
            let mut xs = vec![];
            let mut ys = vec![];
            for (u, v) in [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)] {
                let (x, y) = affine.apply(u, v);
                xs.push((x + 1.0) * 0.5 * framebuffer.w as f32);
                ys.push((y + 1.0) * 0.5 * framebuffer.h as f32);
            }
            let low = |v: &[f32]| v.iter().cloned().fold(f32::INFINITY, f32::min).round() as i32;
            let high =
                |v: &[f32]| v.iter().cloned().fold(f32::NEG_INFINITY, f32::max).round() as i32;

            assert_eq!(
                scissor,
                Rectangle::new(
                    Point::from((low(&xs), low(&ys))),
                    Size::from((high(&xs) - low(&xs), high(&ys) - low(&ys)))
                ),
                "{transform:?}: the scissor and the quad disagree"
            );
        }
    }

    #[test]
    fn an_untransformed_texture_samples_the_whole_source() {
        let affine = texture(
            Rectangle::from_size(Size::from((64.0, 32.0))),
            (64.0, 32.0),
            Transform::Normal,
            false,
        );
        assert!(close(affine.apply(0.0, 0.0), (0.0, 0.0)));
        assert!(close(affine.apply(1.0, 1.0), (1.0, 1.0)));
    }

    #[test]
    fn a_source_rectangle_samples_only_its_part() {
        // The right half of a 64-wide texture.
        let affine = texture(
            Rectangle::new(Point::from((32.0, 0.0)), Size::from((32.0, 32.0))),
            (64.0, 32.0),
            Transform::Normal,
            false,
        );
        assert!(close(affine.apply(0.0, 0.0), (0.5, 0.0)));
        assert!(close(affine.apply(1.0, 0.0), (1.0, 0.0)));
    }

    #[test]
    fn a_flipped_buffer_samples_bottom_up() {
        let affine = texture(
            Rectangle::from_size(Size::from((10.0, 10.0))),
            (10.0, 10.0),
            Transform::Normal,
            true,
        );
        // The top of the quad reads the bottom of the buffer.
        assert!(close(affine.apply(0.0, 0.0), (0.0, 1.0)));
        assert!(close(affine.apply(0.0, 1.0), (0.0, 0.0)));
    }

    #[test]
    fn every_surface_transform_samples_the_whole_texture_exactly_once() {
        for transform in [
            Transform::Normal,
            Transform::_90,
            Transform::_180,
            Transform::_270,
            Transform::Flipped,
            Transform::Flipped90,
            Transform::Flipped180,
            Transform::Flipped270,
        ] {
            let affine = texture(
                Rectangle::from_size(Size::from((16.0, 16.0))),
                (16.0, 16.0),
                transform,
                false,
            );
            let mut corners: Vec<(i32, i32)> = [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)]
                .into_iter()
                .map(|(u, v)| {
                    let (x, y) = affine.apply(u, v);
                    (x.round() as i32, y.round() as i32)
                })
                .collect();
            corners.sort();
            assert_eq!(
                corners,
                vec![(0, 0), (0, 1), (1, 0), (1, 1)],
                "{transform:?} does not sample the texture exactly once"
            );
        }
    }
}
