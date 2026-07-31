// SPDX-License-Identifier: GPL-3.0-or-later
//
// The pointer image. Ports the cursor half of src/input.c.
//
// Two sources, and the compositor has to draw both. A client that has focus
// may set its own cursor surface — a text field asking for an I-beam — and
// everywhere else the compositor draws a theme cursor of its own. wlroots
// packaged both behind wlr_cursor; Smithay does not, so the theme half lives
// here.
//
// The shell is not involved. It cannot be: the pointer has to keep moving
// while the page is busy, and a cursor drawn in DOM would lag a slow frame.

use std::collections::HashMap;

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::utils::{Logical, Physical, Point, Transform};

/// One frame of a themed cursor.
struct Frame {
    buffer: MemoryRenderBuffer,
    /// Where the pointer actually is within the image.
    hotspot: Point<i32, Physical>,
    /// How long this frame lasts, for an animated cursor.
    delay: u32,
}

/// A loaded theme, one cursor at a time.
pub struct Cursor {
    frames: Vec<Frame>,
    /// Total animation length, so the frame for a timestamp is a modulo.
    duration: u32,
}

impl Cursor {
    /// The frame to draw at `millis` since the compositor started.
    ///
    /// A still cursor is one frame with a zero delay, which lands on index 0
    /// for every timestamp without a special case.
    fn frame(&self, millis: u32) -> &Frame {
        if self.duration == 0 {
            return &self.frames[0];
        }
        let mut at = millis % self.duration;
        for frame in &self.frames {
            if at < frame.delay {
                return frame;
            }
            at -= frame.delay;
        }
        // Only reachable if the delays do not sum to the duration.
        &self.frames[self.frames.len() - 1]
    }
}

/// Every cursor that has been asked for, loaded once.
pub struct Theme {
    name: String,
    size: u32,
    loaded: HashMap<(String, i32), Option<Cursor>>,
}

impl Theme {
    /// The theme named by the environment, as every toolkit resolves it.
    pub fn new() -> Self {
        let name = std::env::var("XCURSOR_THEME").unwrap_or_else(|_| "default".to_owned());
        let size = std::env::var("XCURSOR_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(24);
        Self {
            name,
            size,
            loaded: HashMap::new(),
        }
    }

    /// The theme's name, as the settings portal reports it. A toolkit that is
    /// told a different one draws a different cursor from the compositor.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The size in the same units the portal and every toolkit use.
    pub fn size(&self) -> u32 {
        self.size
    }

    /// Load `name` at `scale`, or fall back through the names that mean the
    /// same thing.
    ///
    /// Themes disagree about which name is canonical — a theme with only
    /// `left_ptr` and one with only `default` are both common — so a miss on
    /// the requested name is not the same as having no cursor.
    fn load(&mut self, name: &str, scale: i32) -> Option<&Cursor> {
        let key = (name.to_owned(), scale);
        if !self.loaded.contains_key(&key) {
            let cursor = self.load_uncached(name, scale);
            self.loaded.insert(key.clone(), cursor);
        }
        self.loaded.get(&key).and_then(|c| c.as_ref())
    }

    /// The themes to look in, in order.
    ///
    /// The configured name first, then the same name with spaces as hyphens,
    /// then the usual fallbacks. The hyphen form matters: a theme directory is
    /// `Bibata-Modern-Classic` while XCURSOR_THEME is often set to the
    /// display name `Bibata Modern Classic`, and looking only for the literal
    /// value finds nothing — which presents as a compositor with no pointer
    /// rather than as a misconfigured theme.
    fn theme_names(&self) -> Vec<String> {
        let mut names = vec![self.name.clone()];
        let hyphenated = self.name.replace(' ', "-");
        if hyphenated != self.name {
            names.push(hyphenated);
        }
        for fallback in ["default", "Adwaita", "Vanilla-DMZ"] {
            if !names.iter().any(|n| n == fallback) {
                names.push(fallback.to_owned());
            }
        }
        names
    }

    fn load_uncached(&self, name: &str, scale: i32) -> Option<Cursor> {
        let wanted = (self.size as i32 * scale) as u32;

        let mut names = vec![name];
        for alias in ALIASES {
            if alias.contains(&name) {
                names.extend(alias.iter().copied());
            }
        }

        let path = self.theme_names().into_iter().find_map(|theme_name| {
            let theme = xcursor::CursorTheme::load(&theme_name);
            names.iter().find_map(|n| theme.load_icon(n))
        })?;
        let bytes = std::fs::read(path).ok()?;
        let images = xcursor::parser::parse_xcursor(&bytes)?;

        // The size closest to what was asked for. Themes ship a handful of
        // fixed sizes and rarely the exact one.
        let best = images
            .iter()
            .map(|image| image.size)
            .min_by_key(|size| (*size as i32 - wanted as i32).abs())?;

        let frames: Vec<Frame> = images
            .iter()
            .filter(|image| image.size == best)
            .map(|image| {
                // xcursor decodes to ARGB in native byte order, which on a
                // little-endian machine is B, G, R, A in memory — the same
                // order DRM's ARGB8888 names from the low byte up.
                let buffer = MemoryRenderBuffer::from_slice(
                    &image.pixels_rgba,
                    Fourcc::Argb8888,
                    (image.width as i32, image.height as i32),
                    scale,
                    Transform::Normal,
                    None,
                );
                Frame {
                    buffer,
                    hotspot: (image.xhot as i32, image.yhot as i32).into(),
                    delay: image.delay,
                }
            })
            .collect();

        if frames.is_empty() {
            return None;
        }
        let duration = frames.iter().map(|f| f.delay).sum();
        Some(Cursor { frames, duration })
    }

    /// The buffer and hotspot to draw for `name` at `scale` and `millis`.
    pub fn image(
        &mut self,
        name: &str,
        scale: i32,
        millis: u32,
    ) -> Option<(MemoryRenderBuffer, Point<i32, Physical>)> {
        let cursor = self.load(name, scale)?;
        let frame = cursor.frame(millis);
        Some((frame.buffer.clone(), frame.hotspot))
    }
}

/// Names that different themes use for the same cursor.
///
/// X11's cursor names and CSS's disagree, and a theme usually ships one set
/// or the other. Without this a theme with only `left_ptr` yields no default
/// cursor at all, which presents as a compositor that draws no pointer.
const ALIASES: &[&[&str]] = &[
    &["default", "left_ptr", "arrow", "top_left_arrow"],
    &["text", "xterm", "ibeam"],
    &["pointer", "hand", "hand1", "hand2", "pointing_hand"],
    &["wait", "watch"],
    &["progress", "left_ptr_watch", "half-busy"],
    &["crosshair", "cross"],
    &["move", "fleur", "all-scroll"],
    &["not-allowed", "crossed_circle", "forbidden"],
    &["grab", "openhand"],
    &["grabbing", "closedhand", "dnd-none"],
    &[
        "ns-resize",
        "size_ver",
        "v_double_arrow",
        "sb_v_double_arrow",
    ],
    &[
        "ew-resize",
        "size_hor",
        "h_double_arrow",
        "sb_h_double_arrow",
    ],
];

/// Where the pointer may go: the union of every mapped output.
///
/// A pointer is not clamped to a rectangle but to the outputs themselves, or
/// it can be moved into the gap between two monitors of different heights and
/// vanish. Clamping per output keeps it on a screen someone can see.
pub fn clamp(
    outputs: &[smithay::utils::Rectangle<i32, Logical>],
    from: Point<f64, Logical>,
    to: Point<f64, Logical>,
) -> Point<f64, Logical> {
    if outputs.is_empty() {
        return to;
    }
    // Already on an output: nothing to do.
    if outputs.iter().any(|o| o.to_f64().contains(to)) {
        return to;
    }

    // Otherwise the nearest point on the output the pointer came from, so a
    // move that runs off an edge slides along it rather than jumping to
    // another monitor.
    let origin = outputs
        .iter()
        .find(|o| o.to_f64().contains(from))
        .or_else(|| outputs.first())
        .expect("checked non-empty");

    let max_x = (origin.loc.x + origin.size.w) as f64 - 1.0;
    let max_y = (origin.loc.y + origin.size.h) as f64 - 1.0;
    Point::from((
        to.x.clamp(origin.loc.x as f64, max_x),
        to.y.clamp(origin.loc.y as f64, max_y),
    ))
}

/// Which of the two cursor images is the one on screen.
///
/// A pen and a mouse are two devices sharing one cursor, and each may name its
/// own picture. The pen wins while it is in proximity, because it is the device
/// being used: an application asking for a crosshair means it for the hand that
/// is drawing, and it has said nothing about the mouse.
///
/// `tablet` is `None` once the pen leaves proximity, which is what hands the
/// cursor back rather than stranding a crosshair under a pointer that has moved
/// somewhere else.
pub fn active_image(
    tablet: Option<&smithay::input::pointer::CursorImageStatus>,
    pointer: &smithay::input::pointer::CursorImageStatus,
) -> smithay::input::pointer::CursorImageStatus {
    tablet.cloned().unwrap_or_else(|| pointer.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::utils::Rectangle;

    fn outputs() -> Vec<Rectangle<i32, Logical>> {
        vec![
            Rectangle::new((0, 0).into(), (2560, 1440).into()),
            Rectangle::new((2560, 0).into(), (2560, 1440).into()),
        ]
    }

    #[test]
    fn a_theme_named_with_spaces_is_also_looked_up_hyphenated() {
        // XCURSOR_THEME is commonly set to a display name while the directory
        // on disk uses hyphens. Looking only for the literal value finds
        // nothing and the compositor draws no pointer at all.
        let theme = Theme {
            name: "Bibata Modern Classic".to_owned(),
            size: 24,
            loaded: HashMap::new(),
        };
        let names = theme.theme_names();
        assert_eq!(names[0], "Bibata Modern Classic");
        assert_eq!(names[1], "Bibata-Modern-Classic");
        assert!(names.iter().any(|n| n == "default"), "must still fall back");
    }

    #[test]
    fn the_fallbacks_are_not_duplicated() {
        let theme = Theme {
            name: "default".to_owned(),
            size: 24,
            loaded: HashMap::new(),
        };
        let names = theme.theme_names();
        assert_eq!(names.iter().filter(|n| *n == "default").count(), 1);
    }

    #[test]
    fn a_pointer_on_an_output_is_left_alone() {
        let to = Point::from((100.0, 100.0));
        assert_eq!(clamp(&outputs(), (50.0, 50.0).into(), to), to);
    }

    #[test]
    fn a_pointer_crosses_between_touching_outputs() {
        // The monitors are side by side, so this is inside the second one and
        // must not be pulled back.
        let to = Point::from((3000.0, 700.0));
        assert_eq!(clamp(&outputs(), (2000.0, 700.0).into(), to), to);
    }

    #[test]
    fn a_pointer_leaving_every_output_stays_on_the_one_it_left() {
        // Off the right edge of the layout.
        let clamped = clamp(&outputs(), (5000.0, 700.0).into(), (6000.0, 700.0).into());
        assert_eq!(clamped, Point::from((5119.0, 700.0)));

        // Off the top.
        let clamped = clamp(&outputs(), (100.0, 10.0).into(), (100.0, -50.0).into());
        assert_eq!(clamped, Point::from((100.0, 0.0)));
    }

    #[test]
    fn with_no_outputs_the_pointer_is_not_moved() {
        // Nothing to clamp to, and refusing to move would pin the pointer at
        // the origin for the whole session.
        let to = Point::from((10.0, 10.0));
        assert_eq!(clamp(&[], (0.0, 0.0).into(), to), to);
    }

    #[test]
    fn the_pen_wins_while_it_is_in_proximity() {
        use smithay::input::pointer::CursorImageStatus;
        // A drawing application asking for a crosshair means it for the hand
        // that is drawing. Before this the tool's image was dropped and the
        // ordinary arrow was drawn over the tablet.
        let pointer = CursorImageStatus::default_named();
        let pen = CursorImageStatus::Hidden;
        assert_eq!(
            active_image(Some(&pen), &pointer),
            CursorImageStatus::Hidden
        );
    }

    #[test]
    fn lifting_the_pen_hands_the_cursor_back() {
        use smithay::input::pointer::CursorImageStatus;
        // None is what proximity-out sets. Leaving the pen's choice up would
        // strand it under a pointer that has moved somewhere else entirely.
        let pointer = CursorImageStatus::default_named();
        assert_eq!(active_image(None, &pointer), pointer);
    }

    #[test]
    fn a_pen_that_asked_for_nothing_does_not_hide_the_pointer() {
        use smithay::input::pointer::CursorImageStatus;
        // The two are separate statuses precisely so neither overwrites the
        // other: with no tool image the pointer's own choice is untouched,
        // whatever it happens to be.
        let pointer = CursorImageStatus::Hidden;
        assert_eq!(active_image(None, &pointer), CursorImageStatus::Hidden);
    }
}
