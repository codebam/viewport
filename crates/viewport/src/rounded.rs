// SPDX-License-Identifier: GPL-3.0-or-later
//
// Rounded corners, for an element that has no idea it has any.
//
// The shell draws a window's frame with a `border-radius`, and until this
// existed that radius was invisible: the client's surface is not part of the
// page, the compositor drew it as the rectangle it is, and a square corner
// over a rounded border is a border that was never there.
//
// The obvious way to round a corner is a shader — the fragment picks up an
// alpha from the distance to the corner and the edge comes out smooth. There is
// no such hook here: the DRM backend draws through the Vulkan renderer, which
// has no custom-shader path, and doing it for GLES alone would round the
// corners on the nested backend and leave the real screen square. So the corner
// is cut instead of shaded — the element is drawn as a run of horizontal bands,
// each inset by however much the circle is inset at that height, and the
// corner pixels are simply never painted.
//
// What that costs is antialiasing: the cut is a staircase and not a curve. It
// is a *short* staircase — the shell's border is drawn behind the client with
// the page's own antialiasing, so what steps is the boundary between the client
// and the border, not the outline of the window against the wallpaper — and it
// is one draw call per band rather than one element per band, which is what
// keeps the damage tracker seeing a single window rather than a dozen slivers
// of one.

use smithay::backend::renderer::element::{Element, Id, Kind, RenderElement, UnderlyingStorage};
use smithay::backend::renderer::utils::{CommitCounter, DamageSet, OpaqueRegions};
use smithay::backend::renderer::Renderer;
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer as BufferCoord, Physical, Rectangle, Scale, Transform};

/// The bands one rounded rectangle is drawn as, top to bottom.
///
/// Rows of equal inset are one band: a radius of 8 is 16 rows of corner and
/// five or six distinct insets, so merging them turns sixteen draw calls into
/// six. The middle — everything below the top corner and above the bottom one —
/// is a single band the full width of the rectangle.
///
/// Returns the whole rectangle as one band for a radius of zero or less, which
/// is what makes "no rounding" cost nothing but the wrapper.
pub fn bands(rect: Rectangle<i32, Physical>, radius: i32) -> Vec<Rectangle<i32, Physical>> {
    let radius = radius.min(rect.size.w / 2).min(rect.size.h / 2);
    if radius <= 0 || rect.is_empty() {
        return vec![rect];
    }

    // A corner that insets nothing is not a corner. `inset` rounds to whole
    // pixels, and for a radius of 1 the deepest it ever gets is 0.134 — so the
    // three bands below would be three full-width rectangles drawing exactly
    // what one does, at three times the draw calls.
    if (0..radius).all(|row| inset(radius, row) == 0) {
        return vec![rect];
    }

    let mut bands: Vec<Rectangle<i32, Physical>> = Vec::new();
    let mut push = |y: i32, height: i32, inset: i32| {
        let width = rect.size.w - inset * 2;
        if width > 0 && height > 0 {
            bands.push(Rectangle::new(
                (rect.loc.x + inset, y).into(),
                (width, height).into(),
            ));
        }
    };

    // The top corner, as runs of rows that share an inset.
    let mut run_start = 0;
    let mut run_inset = inset(radius, 0);
    for row in 1..=radius {
        let this = if row == radius {
            -1
        } else {
            inset(radius, row)
        };
        if this != run_inset {
            push(rect.loc.y + run_start, row - run_start, run_inset);
            run_start = row;
            run_inset = this;
        }
    }

    // The middle, full width.
    push(rect.loc.y + radius, rect.size.h - radius * 2, 0);

    // The bottom corner, the top one upside down.
    let bottom = rect.loc.y + rect.size.h;
    let mut run_end = 0;
    let mut run_inset = inset(radius, 0);
    for row in 1..=radius {
        let this = if row == radius {
            -1
        } else {
            inset(radius, row)
        };
        if this != run_inset {
            push(bottom - row, row - run_end, run_inset);
            run_end = row;
            run_inset = this;
        }
    }

    bands
}

/// What a rounding of `radius` cuts *away* from `rect`: the four corner
/// wedges, and nothing else.
///
/// The complement of [`bands`] inside the same rectangle, derived from it
/// rather than computed again — the two have to agree to the pixel, because
/// one is what a client is drawn as and the other is what is drawn behind it.
///
/// This is what the shell's border curve occupies inside the hole it drew. It
/// matters that it is the wedge and not the whole corner square: the rest of
/// that square is the hole, which in the shell's buffer is the desktop's own
/// background, and drawing that over the window a floating one is lifted above
/// is four triangles of wallpaper punched through it.
pub fn cutaway(rect: Rectangle<i32, Physical>, radius: i32) -> Vec<Rectangle<i32, Physical>> {
    let radius = radius.min(rect.size.w / 2).min(rect.size.h / 2);
    if radius <= 0 || rect.is_empty() {
        return Vec::new();
    }

    let right_of = |r: Rectangle<i32, Physical>| r.loc.x + r.size.w;
    let mut wedges = Vec::new();
    for band in bands(rect, radius) {
        // The rows of a band are inset by the same amount on both sides, so
        // what the rounding took is the strip left of the band and the strip
        // right of it.
        let left = band.loc.x - rect.loc.x;
        if left > 0 {
            wedges.push(Rectangle::new(
                (rect.loc.x, band.loc.y).into(),
                (left, band.size.h).into(),
            ));
        }
        let right = right_of(rect) - right_of(band);
        if right > 0 {
            wedges.push(Rectangle::new(
                (right_of(band), band.loc.y).into(),
                (right, band.size.h).into(),
            ));
        }
    }
    wedges
}

/// How far in from the edge the circle is at `row` rows from the top of the
/// corner.
///
/// Measured at the middle of the row rather than its edge, so a radius of 1
/// clips the single corner pixel rather than nothing or a whole row, and the
/// steps land where a curve drawn through them would.
fn inset(radius: i32, row: i32) -> i32 {
    let r = f64::from(radius);
    let dy = r - (f64::from(row) + 0.5);
    let chord = (r * r - dy * dy).max(0.0).sqrt();
    ((r - chord).round() as i32).clamp(0, radius)
}

/// An element with the corners of `rect` cut off it.
///
/// The rectangle is in the same space as the wrapped element's geometry, which
/// for a window is the box the shell drew for it rather than the surface: a
/// client with shadows draws outside its own window, and rounding the shadow
/// rounds nothing anyone can see. Everything outside the rectangle is dropped
/// along with the corners — a rounded window that keeps its square shadow is
/// the same square corner over the same border.
#[derive(Debug)]
pub struct RoundedRenderElement<E> {
    element: E,
    /// The wrapped element's own geometry, at the scale this was built with.
    ///
    /// Kept whole rather than narrowed to the rounded shape, because it is the
    /// rectangle the element's `src` covers: the renderer is handed the pair
    /// and stretches one onto the other, so a geometry that is not what `src`
    /// describes is a window drawn at the wrong magnification. What the corner
    /// takes away is expressed in the bands and nowhere else.
    geometry: Rectangle<i32, Physical>,
    /// Absolute, in the same space as `geometry`, and already clipped to it.
    ///
    /// Shared rather than owned: see [`shape`]. Every surface of every window
    /// is wrapped again on every output on every frame, and for a window that
    /// is not moving the answer is the same every time.
    bands: Shape,
    /// The largest rectangles wholly inside the rounded shape, for the opaque
    /// region. Three of them: the middle full-width band and the two
    /// corner bands inset by the radius.
    solid: Shape,
}

/// One of the two rectangle lists a rounded element is built from, shared
/// between every element built from the same inputs.
type Shape = std::rc::Rc<Vec<Rectangle<i32, Physical>>>;

thread_local! {
    /// The bands and the solid parts, by what they were computed from.
    ///
    /// Both lists depend on nothing but the rectangle, the radius and the
    /// element's geometry, and all three are constant for a window nobody is
    /// dragging — but `from_element` runs inside the render loop's `filter_map`
    /// once per surface per output per frame, so a still desktop was allocating
    /// two vectors per surface at the refresh rate for two answers it already
    /// had.
    ///
    /// Per thread and unsynchronised because rendering is: the compositor draws
    /// from one thread and the map never leaves it.
    static SHAPES: std::cell::RefCell<std::collections::HashMap<ShapeKey, (Shape, Shape)>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Everything the two lists are a function of, flattened into something
/// hashable — `Rectangle` is neither `Hash` nor `Ord`.
///
/// The rectangle, the radius, and the geometry the bands were clipped to.
type ShapeKey = [i32; 9];

/// How many shapes to keep. A desk with a window per column on four screens is
/// a few dozen live keys; past this the map is holding rectangles for windows
/// that have moved on, so it is dropped whole rather than aged entry by entry.
const SHAPE_LIMIT: usize = 256;

/// The bands and the solid parts for these inputs, computed once.
fn shape(
    rect: Rectangle<i32, Physical>,
    radius: i32,
    geometry: Rectangle<i32, Physical>,
) -> (Shape, Shape) {
    let key: ShapeKey = [
        rect.loc.x,
        rect.loc.y,
        rect.size.w,
        rect.size.h,
        radius,
        geometry.loc.x,
        geometry.loc.y,
        geometry.size.w,
        geometry.size.h,
    ];
    SHAPES.with(|shapes| {
        let mut shapes = shapes.borrow_mut();
        if let Some(found) = shapes.get(&key) {
            return found.clone();
        }

        // Cut from the rectangle and then clipped to the element, rather than
        // rounding whatever the two happen to share: a window scrolled half
        // off its output has a geometry that is not the box the shell drew,
        // and the corner belongs to the box.
        let cut: Vec<_> = bands(rect, radius)
            .into_iter()
            .filter_map(|band| band.intersection(geometry))
            .filter(|band| !band.is_empty())
            .collect();

        let radius = radius.min(rect.size.w / 2).min(rect.size.h / 2).max(0);
        let solid: Vec<_> = [
            Rectangle::new(
                (rect.loc.x, rect.loc.y + radius).into(),
                (rect.size.w, rect.size.h - radius * 2).into(),
            ),
            Rectangle::new(
                (rect.loc.x + radius, rect.loc.y).into(),
                (rect.size.w - radius * 2, radius).into(),
            ),
            Rectangle::new(
                (rect.loc.x + radius, rect.loc.y + rect.size.h - radius).into(),
                (rect.size.w - radius * 2, radius).into(),
            ),
        ]
        .into_iter()
        .filter_map(|part| part.intersection(geometry))
        .filter(|part| !part.is_empty())
        .collect();

        if shapes.len() >= SHAPE_LIMIT {
            shapes.clear();
        }
        let shape = (std::rc::Rc::new(cut), std::rc::Rc::new(solid));
        shapes.insert(key, shape.clone());
        shape
    })
}

impl<E: Element> RoundedRenderElement<E> {
    /// Round `element` to `radius` inside `rect`.
    ///
    /// `None` when the element and the rectangle do not meet — the same answer
    /// `CropRenderElement` gives, and for the same reason: there is nothing to
    /// draw and the caller should not add an element for it.
    pub fn from_element(
        element: E,
        scale: impl Into<Scale<f64>>,
        rect: Rectangle<i32, Physical>,
        radius: i32,
    ) -> Option<Self> {
        let scale = scale.into();
        let geometry = element.geometry(scale);
        if geometry.is_empty() {
            return None;
        }

        let (bands, solid) = shape(rect, radius, geometry);
        if bands.is_empty() {
            return None;
        }

        Some(RoundedRenderElement {
            element,
            geometry,
            bands,
            solid,
        })
    }

    /// Draw `element`, but only the rectangles in `bands`.
    ///
    /// The other constructor is handed a shape and cuts corners out of it;
    /// this one is handed the pieces directly, for the caller that already
    /// knows exactly which slivers it wants — see [`cutaway`], which is the
    /// only one.
    ///
    /// Nothing is claimed as opaque. The pieces this is used for are the
    /// antialiased edge of the shell's own border, which is translucent at the
    /// curve; promising the renderer it is solid would let it skip drawing
    /// what is underneath.
    pub fn from_bands(
        element: E,
        scale: impl Into<Scale<f64>>,
        bands: Vec<Rectangle<i32, Physical>>,
    ) -> Option<Self> {
        let geometry = element.geometry(scale.into());
        let bands: Vec<_> = bands
            .into_iter()
            .filter_map(|band| band.intersection(geometry))
            .filter(|band| !band.is_empty())
            .collect();
        if geometry.is_empty() || bands.is_empty() {
            return None;
        }
        Some(RoundedRenderElement {
            element,
            geometry,
            bands: std::rc::Rc::new(bands),
            solid: std::rc::Rc::new(Vec::new()),
        })
    }

    /// Whether this element still draws a plain rectangle.
    ///
    /// True for a radius that cut nothing — `bands` answers the whole
    /// rectangle for a radius of zero, and for one too small to inset a single
    /// pixel — and that is the case where everything a rounded element does is
    /// pass the wrapped one through.
    fn is_rectangular(&self) -> bool {
        matches!(self.bands.as_slice(), [only] if *only == self.geometry)
    }

    /// A rectangle of `self.geometry` moved into the `dst` the renderer handed
    /// us.
    ///
    /// The two are the same rectangle for an element drawn where it is, and
    /// differ when something outside has scaled it — the overview draws every
    /// window as a thumbnail, and it wraps this rather than the other way
    /// round, because the corner is a property of the window and not of how
    /// large it is being shown. Mapping through the ratio is what lets one set
    /// of bands serve both.
    fn to_dst(
        &self,
        part: Rectangle<i32, Physical>,
        dst: Rectangle<i32, Physical>,
    ) -> Rectangle<i32, Physical> {
        if dst == self.geometry {
            return part;
        }
        let sx = f64::from(dst.size.w) / f64::from(self.geometry.size.w.max(1));
        let sy = f64::from(dst.size.h) / f64::from(self.geometry.size.h.max(1));
        let left = f64::from(part.loc.x - self.geometry.loc.x) * sx;
        let top = f64::from(part.loc.y - self.geometry.loc.y) * sy;
        let right = left + f64::from(part.size.w) * sx;
        let bottom = top + f64::from(part.size.h) * sy;
        // Rounded as edges rather than as an origin and a size, so two bands
        // that touched before the scale still touch after it and no seam of
        // wallpaper opens up between them.
        let (left, top) = (left.round() as i32, top.round() as i32);
        let (right, bottom) = (right.round() as i32, bottom.round() as i32);
        Rectangle::new(
            (dst.loc.x + left, dst.loc.y + top).into(),
            (right - left, bottom - top).into(),
        )
    }
}

impl<E: Element> Element for RoundedRenderElement<E> {
    fn id(&self) -> &Id {
        self.element.id()
    }

    fn current_commit(&self) -> CommitCounter {
        self.element.current_commit()
    }

    fn src(&self) -> Rectangle<f64, BufferCoord> {
        self.element.src()
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        // The element's own, corners included. See the note on the field: this
        // rectangle is one half of the pair the renderer stretches, and the
        // corner is not a change of shape but a set of pieces left unpainted.
        self.element.geometry(scale)
    }

    fn transform(&self) -> Transform {
        self.element.transform()
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        // Unchanged: damage is relative to a geometry this element did not
        // narrow. A corner reported as damaged and then not painted is a
        // corner the tracker lets whatever is behind it repaint, which is
        // exactly what has to happen there.
        self.element.damage_since(scale, commit)
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        let origin = self.element.geometry(scale).loc;
        // Only what is both opaque in the element and inside the rounded
        // shape. Claiming a corner is opaque when it is never painted leaves
        // whatever is behind it culled and the corner full of the last frame.
        self.element
            .opaque_regions(scale)
            .into_iter()
            .flat_map(|rect| {
                self.solid.iter().filter_map(move |solid| {
                    let mut solid = *solid;
                    solid.loc -= origin;
                    rect.intersection(solid)
                })
            })
            .filter(|rect| !rect.is_empty())
            .collect()
    }

    fn alpha(&self) -> f32 {
        self.element.alpha()
    }

    fn kind(&self) -> Kind {
        self.element.kind()
    }

    fn is_framebuffer_effect(&self) -> bool {
        self.element.is_framebuffer_effect()
    }
}

impl<R: Renderer, E: RenderElement<R>> RenderElement<R> for RoundedRenderElement<E> {
    fn draw(
        &self,
        frame: &mut R::Frame<'_, '_>,
        src: Rectangle<f64, BufferCoord>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), R::Error> {
        for piece in self.pieces(src, dst, damage, opaque_regions) {
            self.element.draw(
                frame,
                piece.src,
                piece.dst,
                &piece.damage,
                &piece.opaque,
                cache,
            )?;
        }
        Ok(())
    }

    /// The buffer behind this element — but only when this element is not
    /// actually rounding anything.
    ///
    /// This is what a compositor asks before putting an element on a hardware
    /// plane, and a plane draws a rectangle: the scanout hardware takes a
    /// buffer, a source rectangle and a destination rectangle, and there is
    /// nowhere in that to say "with the corners taken off". An element that
    /// offers its buffer here is one the DRM backend may hand straight to the
    /// display controller, and the band-splitting in `draw` — which is the
    /// whole of the rounding — never runs.
    ///
    /// That is why a rounded window came out square on a real screen and round
    /// everywhere else: on the headless and winit backends every element is
    /// composited, so the corners were always cut, and on DRM a window that
    /// was a candidate for a plane was scanned out whole. It is also why a
    /// second window on the workspace "fixed" it — two overlapping windows are
    /// a worse fit for the planes, so both got composited and both got their
    /// corners.
    ///
    /// Declining costs a rounded window its direct scanout. That is not a
    /// choice this can make differently: a window with round corners is not a
    /// rectangle, and the one case where scanout matters most — a fullscreen
    /// window — is drawn square anyway, so it still takes the plane.
    fn underlying_storage(&self, renderer: &mut R) -> Option<UnderlyingStorage<'_>> {
        if !self.is_rectangular() {
            return None;
        }
        self.element.underlying_storage(renderer)
    }

    fn capture_framebuffer(
        &self,
        frame: &mut R::Frame<'_, '_>,
        src: Rectangle<f64, BufferCoord>,
        dst: Rectangle<i32, Physical>,
        cache: &UserDataMap,
    ) -> Result<(), R::Error> {
        self.element.capture_framebuffer(frame, src, dst, cache)
    }
}

/// One band as the renderer is asked to draw it.
///
/// A band is handed to the wrapped element as though it were the whole of a
/// smaller element — its own piece of the source, its own destination, and
/// damage and opaque regions relative to it — because that is the only shape
/// `draw` takes.
#[derive(Debug, PartialEq)]
pub struct Piece {
    pub src: Rectangle<f64, BufferCoord>,
    pub dst: Rectangle<i32, Physical>,
    pub damage: Vec<Rectangle<i32, Physical>>,
    pub opaque: Vec<Rectangle<i32, Physical>>,
}

impl<E: Element> RoundedRenderElement<E> {
    /// Split a draw into one piece per band.
    ///
    /// Separate from `draw` so it can be tested without a renderer: the
    /// arithmetic is the whole of what can go wrong here, and getting it wrong
    /// shows up as a window drawn at the wrong magnification or a seam of
    /// wallpaper across it — neither of which a type checks.
    ///
    /// A band with nothing damaged in it is left out: drawing it would repaint
    /// pixels nobody said had changed.
    pub fn pieces(
        &self,
        src: Rectangle<f64, BufferCoord>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
    ) -> Vec<Piece> {
        let transform = self.element.transform();
        // Damage and opaque regions arrive relative to `dst`; the bands are
        // absolute. One of the two has to move, and moving the two lists once
        // is cheaper than moving every band back.
        let relative_to = |rects: &[Rectangle<i32, Physical>], band: Rectangle<i32, Physical>| {
            rects
                .iter()
                .filter_map(|rect| {
                    let mut rect = *rect;
                    rect.loc += dst.loc;
                    rect.intersection(band).map(|mut hit| {
                        hit.loc -= band.loc;
                        hit
                    })
                })
                .collect::<Vec<_>>()
        };

        let mut pieces = Vec::with_capacity(self.bands.len());
        for band in self.bands.iter() {
            let Some(band) = self.to_dst(*band, dst).intersection(dst) else {
                continue;
            };
            if band.is_empty() {
                continue;
            }
            let damage = relative_to(damage, band);
            if damage.is_empty() {
                continue;
            }
            pieces.push(Piece {
                src: sub_src(src, dst, transform, band),
                dst: band,
                damage,
                opaque: relative_to(opaque_regions, band),
            });
        }
        pieces
    }
}

/// The part of `src` that lands in `part`, where `src` as a whole lands in
/// `dst`.
///
/// The same arithmetic `CropRenderElement` does when it is built, done here
/// per band instead: the source rectangle is in buffer coordinates and the
/// destination in physical ones, and the transform between them is the
/// client's own — a rotated buffer maps its top edge somewhere other than the
/// top.
fn sub_src(
    src: Rectangle<f64, BufferCoord>,
    dst: Rectangle<i32, Physical>,
    transform: Transform,
    part: Rectangle<i32, Physical>,
) -> Rectangle<f64, BufferCoord> {
    let mut relative = part;
    relative.loc -= dst.loc;

    let physical_to_buffer = src.size / transform.invert().transform_size(dst.size).to_f64();
    let mut sub = relative.to_f64().to_logical(1.0).to_buffer(
        physical_to_buffer,
        transform,
        &dst.size.to_f64().to_logical(1.0),
    );
    sub.loc += src.loc;
    sub
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, Physical> {
        Rectangle::new((x, y).into(), (w, h).into())
    }

    /// Enough of an element to be wrapped: a rectangle on screen and the piece
    /// of a buffer that fills it. Nothing here draws, and `pieces` is the part
    /// that has to be right whatever does.
    #[derive(Debug)]
    struct Fake {
        id: Id,
        geometry: Rectangle<i32, Physical>,
        opaque: Vec<Rectangle<i32, Physical>>,
    }

    impl Fake {
        fn new(geometry: Rectangle<i32, Physical>) -> Self {
            Self {
                id: Id::new(),
                geometry,
                opaque: vec![Rectangle::from_size(geometry.size)],
            }
        }
    }

    impl Element for Fake {
        fn id(&self) -> &Id {
            &self.id
        }
        fn current_commit(&self) -> CommitCounter {
            CommitCounter::default()
        }
        fn src(&self) -> Rectangle<f64, BufferCoord> {
            Rectangle::from_size(
                (
                    f64::from(self.geometry.size.w),
                    f64::from(self.geometry.size.h),
                )
                    .into(),
            )
        }
        fn geometry(&self, _scale: Scale<f64>) -> Rectangle<i32, Physical> {
            self.geometry
        }
        fn opaque_regions(&self, _scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
            self.opaque.iter().copied().collect()
        }
    }

    /// A rounded element must not offer its buffer for direct scanout, and a
    /// square one must.
    ///
    /// This is the whole of the bug where a window came out square on a real
    /// screen and round everywhere else: a plane draws a rectangle, so a
    /// compositor that puts a rounded window on one gets the buffer as it is
    /// and none of the band splitting below. Headless and nested composite
    /// everything, which is why nothing here ever saw it.
    #[test]
    fn only_a_square_element_may_be_scanned_out() {
        let geometry = rect(0, 0, 200, 100);

        let square = RoundedRenderElement::from_element(Fake::new(geometry), 1.0, geometry, 0)
            .expect("a square element");
        assert!(square.is_rectangular(), "a radius of zero cuts nothing");

        let round = RoundedRenderElement::from_element(Fake::new(geometry), 1.0, geometry, 12)
            .expect("a rounded element");
        assert!(!round.is_rectangular(), "a corner is not a rectangle");

        // A radius too small to inset a pixel is square in fact, whatever it
        // says, and there is nothing to be gained by refusing the plane.
        let hair = RoundedRenderElement::from_element(Fake::new(geometry), 1.0, geometry, 1)
            .expect("an element rounded by nothing");
        assert!(hair.is_rectangular());

        // Cut to something smaller than the element is not a rectangle either:
        // a plane would draw the part that was meant to be dropped.
        let cropped =
            RoundedRenderElement::from_element(Fake::new(geometry), 1.0, rect(10, 10, 100, 50), 0)
                .expect("an element with its edges cut off");
        assert!(!cropped.is_rectangular());

        // And the wedges are never a rectangle.
        let wedges =
            RoundedRenderElement::from_bands(Fake::new(geometry), 1.0, cutaway(geometry, 12))
                .expect("the corners of a border");
        assert!(!wedges.is_rectangular());
    }

    /// What the rounding cuts away is exactly what it does not keep: every
    /// wedge is inside the rectangle, none of them overlaps a band, and
    /// together the two cover the whole of it.
    ///
    /// That is the property the bug turned on. The corners used to be drawn as
    /// whole squares, which is the wedge *plus* the part of the hole the
    /// client covers — and where the client fell short of its hole, as every
    /// terminal does by a few pixels of cell rounding, what showed through was
    /// the desktop's own background over the window underneath.
    #[test]
    fn the_cutaway_is_everything_the_rounding_does_not_keep() {
        let rect = Rectangle::<i32, Physical>::new((100, 100).into(), (60, 40).into());
        let radius = 10;
        let kept = bands(rect, radius);
        let cut = cutaway(rect, radius);
        assert!(!cut.is_empty());

        let area = |rects: &[Rectangle<i32, Physical>]| -> i32 {
            rects.iter().map(|r| r.size.w * r.size.h).sum()
        };
        assert_eq!(
            area(&kept) + area(&cut),
            rect.size.w * rect.size.h,
            "the two halves are the whole rectangle"
        );
        for wedge in &cut {
            assert_eq!(wedge.intersection(rect), Some(*wedge), "inside the rect");
            for band in &kept {
                assert!(
                    wedge.intersection(*band).is_none_or(|hit| hit.is_empty()),
                    "{wedge:?} overlaps {band:?}"
                );
            }
        }
    }

    /// The wedges sit in the corners and nowhere else: nothing is cut from the
    /// middle of an edge, which would be a bite out of the border.
    #[test]
    fn the_cutaway_is_only_corners() {
        let rect = Rectangle::<i32, Physical>::new((0, 0).into(), (60, 40).into());
        let radius = 8;
        for wedge in cutaway(rect, radius) {
            let top = wedge.loc.y < radius;
            let bottom = wedge.loc.y + wedge.size.h > rect.size.h - radius;
            let left = wedge.loc.x < radius;
            let right = wedge.loc.x + wedge.size.w > rect.size.w - radius;
            assert!(top || bottom, "{wedge:?} is not in a corner row");
            assert!(left || right, "{wedge:?} is not in a corner column");
        }
    }

    /// A square window cuts nothing away, so there is nothing to draw behind
    /// it — and asking costs no rectangles at all.
    #[test]
    fn a_square_window_has_no_cutaway() {
        let rect = Rectangle::<i32, Physical>::new((0, 0).into(), (60, 40).into());
        assert!(cutaway(rect, 0).is_empty());
        assert!(cutaway(rect, -4).is_empty());
        assert!(cutaway(Rectangle::default(), 10).is_empty());
    }

    /// A radius larger than the window is clamped to half of it, exactly as
    /// the bands are — the two are the same shape read from opposite sides.
    #[test]
    fn a_cutaway_larger_than_the_window_is_clamped() {
        let rect = Rectangle::<i32, Physical>::new((0, 0).into(), (40, 24).into());
        let cut = cutaway(rect, 400);
        let kept = bands(rect, 400);
        let area = |rects: &[Rectangle<i32, Physical>]| -> i32 {
            rects.iter().map(|r| r.size.w * r.size.h).sum()
        };
        assert_eq!(area(&cut) + area(&kept), rect.size.w * rect.size.h);
        for wedge in cut {
            assert!(wedge.size.w <= rect.size.w / 2, "{wedge:?}");
            assert!(wedge.size.h <= rect.size.h / 2, "{wedge:?}");
        }
    }

    fn rounded(geometry: Rectangle<i32, Physical>, radius: i32) -> RoundedRenderElement<Fake> {
        RoundedRenderElement::from_element(Fake::new(geometry), 1.0, geometry, radius)
            .expect("the element is its own rectangle")
    }

    /// The pieces of a whole-element draw put the buffer back together: each
    /// band takes the part of the source that lies under it, at the same
    /// magnification, and no two overlap.
    #[test]
    fn every_piece_samples_the_part_of_the_buffer_under_it() {
        let geometry = rect(40, 30, 120, 90);
        let element = rounded(geometry, 8);
        let damage = [Rectangle::from_size(geometry.size)];
        let pieces = element.pieces(element.element.src(), geometry, &damage, &[]);

        assert!(pieces.len() > 1, "a rounded element is drawn in pieces");
        for piece in &pieces {
            // The buffer is the same size as the element here, so the piece of
            // one is the piece of the other — offset by where the element is.
            assert_eq!(piece.src.loc.x, f64::from(piece.dst.loc.x - geometry.loc.x));
            assert_eq!(piece.src.loc.y, f64::from(piece.dst.loc.y - geometry.loc.y));
            assert_eq!(piece.src.size.w, f64::from(piece.dst.size.w));
            assert_eq!(piece.src.size.h, f64::from(piece.dst.size.h));
            // Damage covered everything, so each band is damaged in full.
            assert_eq!(
                piece.damage,
                vec![Rectangle::from_size(piece.dst.size)],
                "a band is damaged over the whole of itself"
            );
        }
    }

    /// Under a thumbnail the destination is not the geometry, and the bands
    /// have to follow it without leaving a seam: the bottom of one piece is
    /// the top of the next.
    #[test]
    fn a_scaled_draw_leaves_no_seam_between_the_bands() {
        let geometry = rect(0, 0, 100, 100);
        let element = rounded(geometry, 10);
        let dst = rect(500, 200, 50, 50);
        let damage = [Rectangle::from_size(dst.size)];
        let pieces = element.pieces(element.element.src(), dst, &damage, &[]);

        let mut rows: Vec<_> = pieces.iter().map(|p| (p.dst.loc.y, p.dst.size.h)).collect();
        rows.sort();
        assert_eq!(rows.first().map(|r| r.0), Some(dst.loc.y));
        for pair in rows.windows(2) {
            assert_eq!(pair[0].0 + pair[0].1, pair[1].0, "the bands meet: {rows:?}");
        }
        let (last_y, last_h) = *rows.last().expect("there are bands");
        assert_eq!(last_y + last_h, dst.loc.y + dst.size.h);
        for piece in &pieces {
            assert!(piece.dst.size.w <= dst.size.w);
            assert!(piece.dst.loc.x >= dst.loc.x);
        }
    }

    /// A band nothing has damaged is not drawn at all — the point of the
    /// tracker is that an unchanged corner costs nothing.
    #[test]
    fn an_undamaged_band_is_not_drawn() {
        let geometry = rect(0, 0, 100, 100);
        let element = rounded(geometry, 10);
        // One row in the middle of the element, relative to it.
        let damage = [rect(0, 50, 100, 1)];
        let pieces = element.pieces(element.element.src(), geometry, &damage, &[]);
        assert_eq!(pieces.len(), 1, "only the band holding the damage");
        assert_eq!(
            pieces[0].damage,
            vec![rect(0, 50 - pieces[0].dst.loc.y, 100, 1)]
        );
    }

    /// The corners are never claimed as opaque. Whatever is behind them has to
    /// be drawn, and the tracker culls anything it is told is covered.
    #[test]
    fn the_corners_are_not_opaque() {
        let geometry = rect(0, 0, 100, 100);
        let element = rounded(geometry, 10);
        let opaque = element.opaque_regions(1.0.into());
        assert!(!opaque.is_empty(), "the middle of a window is still opaque");
        for region in opaque.iter() {
            // No opaque region reaches the very corner of the element.
            assert!(
                region.loc.x >= 10 || region.loc.y >= 10,
                "an opaque region covers the top-left corner: {region:?}"
            );
        }
    }

    /// The whole rectangle, once, and no arithmetic: square corners are the
    /// old behaviour and have to cost nothing.
    #[test]
    fn no_radius_is_one_band() {
        assert_eq!(bands(rect(0, 0, 100, 50), 0), vec![rect(0, 0, 100, 50)]);
    }

    /// A radius too small to inset anything is no radius at all. `inset` works
    /// in whole pixels and never reaches one for a radius of 1, so the three
    /// bands it used to produce were three full-width rectangles drawing what
    /// one draws.
    #[test]
    fn a_radius_that_cuts_nothing_is_one_band() {
        assert_eq!(inset(1, 0), 0);
        assert_eq!(bands(rect(0, 0, 100, 50), 1), vec![rect(0, 0, 100, 50)]);
    }

    /// The same inputs give the same lists, and the lists are shared rather
    /// than built again — which is the whole point of the cache, since this
    /// runs per surface per output per frame.
    #[test]
    fn the_same_shape_is_computed_once() {
        let geometry = rect(11, 22, 130, 70);
        let first = rounded(geometry, 8);
        let second = rounded(geometry, 8);
        assert!(std::rc::Rc::ptr_eq(&first.bands, &second.bands));
        assert!(std::rc::Rc::ptr_eq(&first.solid, &second.solid));
        // A different radius is a different shape, not a stale hit.
        let third = rounded(geometry, 12);
        assert_ne!(*third.bands, *first.bands);
    }

    /// Every row of the rectangle is covered exactly once, whatever the
    /// radius: a row drawn twice is a translucent window drawn twice, and a
    /// row missed is a stripe of wallpaper through the middle of a client.
    #[test]
    fn the_bands_tile_the_rectangle() {
        for radius in 1..=16 {
            let bands = bands(rect(10, 20, 120, 80), radius);
            for row in 20..100 {
                let covering: Vec<_> = bands
                    .iter()
                    .filter(|b| b.loc.y <= row && row < b.loc.y + b.size.h)
                    .collect();
                assert_eq!(
                    covering.len(),
                    1,
                    "radius {radius}, row {row} covered {} times",
                    covering.len()
                );
            }
        }
    }

    /// The corner is cut, and cut symmetrically: the first row of an 8px
    /// radius is inset by most of it, the row at the radius by none.
    #[test]
    fn the_corner_narrows_toward_the_edge() {
        let bands = bands(rect(0, 0, 100, 100), 8);
        let width_at = |row: i32| {
            bands
                .iter()
                .find(|b| b.loc.y <= row && row < b.loc.y + b.size.h)
                .map(|b| (b.loc.x, b.size.w))
                .expect("every row is covered")
        };
        let (first_x, first_w) = width_at(0);
        let (mid_x, mid_w) = width_at(50);
        assert!(first_w < mid_w, "the top row is narrower than the middle");
        assert_eq!(first_x + first_w, 100 - first_x, "inset on both sides");
        assert_eq!((mid_x, mid_w), (0, 100), "the middle is the full width");
        // Top and bottom are mirror images.
        assert_eq!(width_at(99), (first_x, first_w));
        assert!(width_at(1).1 >= first_w);
    }

    /// A radius larger than the rectangle is clamped rather than turning the
    /// window inside out. The shell can be told any number; half the shorter
    /// side is a circle and there is nothing past it.
    #[test]
    fn a_radius_larger_than_the_box_is_clamped() {
        let bands = bands(rect(0, 0, 20, 10), 400);
        assert!(!bands.is_empty());
        for band in &bands {
            assert!(band.size.w > 0 && band.size.h > 0);
            assert!(band.loc.x >= 0 && band.loc.x + band.size.w <= 20);
            assert!(band.loc.y >= 0 && band.loc.y + band.size.h <= 10);
        }
    }
}
