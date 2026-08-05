// SPDX-License-Identifier: GPL-3.0-or-later
//
// What one output draws, independent of what draws it.
//
// There are two backends — DRM with the Vulkan renderer, and winit with GLES —
// and until this existed only the first drew the desktop. The second showed
// windows on a flat colour, because the element list was assembled inside the
// DRM path against one concrete renderer. Nested is where most development
// happens, so "it looks nothing like the real thing" is expensive.
//
// The split is: [`Frame`] describes what should appear and is worked out with
// no renderer at all, and [`build`] turns that into elements using whichever
// renderer the backend has. Everything that needs a texture — importing the
// shell's buffer, a client's surface, the cursor image — happens on the second
// side, and everything that needs the compositor's state happens on the first.

use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::renderer::element::memory::{
    MemoryRenderBuffer, MemoryRenderBufferRenderElement,
};
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::texture::TextureRenderElement;
use smithay::backend::renderer::element::utils::{CropRenderElement, RescaleRenderElement};
use smithay::backend::renderer::element::{AsRenderElements as _, Id, Kind};
use smithay::backend::renderer::utils::DamageSnapshot;
use smithay::backend::renderer::{ImportAll, ImportDma, ImportMem, Renderer, RendererSuper};
use smithay::desktop::{LayerSurface, Window};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Buffer as BufferCoord, Logical, Physical, Point, Rectangle, Transform};

use crate::rounded::RoundedRenderElement;

smithay::backend::renderer::element::render_elements! {
    /// Everything one output draws.
    ///
    /// Generic over the renderer so both backends share it. The shell is not a
    /// Wayland client — it is a texture imported straight from WebKit's
    /// DMA-BUF — and a window may be cropped to the hole the shell drew for
    /// it, so no single element type covers the list.
    pub OutputElement<R> where R: ImportAll + ImportMem;
    Surface=WaylandSurfaceRenderElement<R>,
    CroppedSurface=CropRenderElement<WaylandSurfaceRenderElement<R>>,
    /// A window with the corners of the shell's border cut out of it. Always
    /// cropped first, because rounding a window is also a promise that nothing
    /// of it is drawn outside the box the shell measured.
    RoundedSurface=RoundedRenderElement<CropRenderElement<WaylandSurfaceRenderElement<R>>>,
    /// A window drawn smaller than it is: the overview, where every window on
    /// every workspace is on screen at once and no client is asked to resize
    /// itself into a thumbnail.
    ScaledSurface=RescaleRenderElement<CropRenderElement<WaylandSurfaceRenderElement<R>>>,
    ScaledRoundedSurface=RescaleRenderElement<RoundedRenderElement<CropRenderElement<WaylandSurfaceRenderElement<R>>>>,
    ScaledWholeSurface=RescaleRenderElement<WaylandSurfaceRenderElement<R>>,
    Shell=TextureRenderElement<<R as RendererSuper>::TextureId>,
    /// A piece of the shell drawn above the windows: the frame around a
    /// floating window, or a dialog the shell put up.
    CroppedShell=CropRenderElement<TextureRenderElement<<R as RendererSuper>::TextureId>>,
    /// A side of a floating window's border, with the outside of the corner
    /// taken off it. Without this the square end of a rounded border paints
    /// the desktop's own background over whatever the window is floating
    /// above.
    RoundedShell=RoundedRenderElement<CropRenderElement<TextureRenderElement<<R as RendererSuper>::TextureId>>>,
    Cursor=MemoryRenderBufferRenderElement<R>,
}

/// The pointer image, resolved but not yet imported.
#[derive(Default)]
pub enum Cursor {
    /// Nothing to draw: a client asked for the pointer to be hidden, or it is
    /// on another output.
    #[default]
    Hidden,
    /// A client's own surface, and the hotspot it declared.
    Surface(WlSurface, Point<i32, Physical>),
    /// A themed image, already loaded.
    Image(MemoryRenderBuffer, Point<i32, Physical>),
}

/// The shell's frame: one buffer spanning the whole output layout.
pub struct Shell {
    /// The compositor's own copy — see `ViewportState::import_shell_frame`.
    pub buffer: Dmabuf,
    /// Minus this output's position, so an output at x=2560 shows the part of
    /// the buffer starting there.
    pub location: Point<f64, Physical>,
    /// What changed since the last frame. Without it a stable element id means
    /// the tracker is told nothing ever changes and the output goes quiet.
    pub damage: DamageSnapshot<i32, BufferCoord>,
    /// Stable for the life of the compositor, for the same reason.
    pub id: Id,
}

/// One window, and everything about drawing it that is not the window itself.
pub struct WindowFrame {
    pub window: Window,
    pub location: Point<i32, Physical>,
    /// The hole the shell drew, which the surface is cropped to.
    pub clip: Option<Rectangle<i32, Physical>>,
    /// The window's own top-left corner, which is what a shrunken window is
    /// scaled about.
    ///
    /// Not `location`: that is where the *surface* starts, and a client drawing
    /// its own shadows starts its surface outside its window — 26 pixels out on
    /// every side, for the ones in this session's log. Scaling about the
    /// surface origin moves a thumbnail up and left by the shadow times one
    /// minus the scale, which is a strip of window hanging outside the box the
    /// shell drew for it.
    pub origin: Point<i32, Physical>,
    /// Drawn this much smaller than the client actually is, about the window's
    /// own top-left corner.
    ///
    /// The overview is the whole reason it exists: every window on every
    /// workspace at once, without asking a client to resize itself into a
    /// thumbnail — which many would refuse, having a minimum size larger than
    /// one. 1.0 for everything else.
    pub scale: f64,
    /// What the shell asked this window to be drawn at, 0.0 to 1.0.
    pub opacity: f32,
    /// The shell's border around this window, drawn immediately above it: the
    /// four sides, and not the middle.
    ///
    /// Only for a window that is over another one — a floating window — where
    /// the border falls inside the window beneath and would otherwise be
    /// covered by that client's surface. Above *this* window rather than above
    /// everything, so two floating windows stack the way they are stacked: the
    /// lower one's border stays under the upper one's surface, which is where
    /// it belongs.
    pub overlay: Vec<(Id, Rectangle<i32, Physical>)>,
    /// The box the shell drew for this window, and the corner radius to cut
    /// out of it — both in physical pixels on this output.
    ///
    /// `None` for a square window, which is a radius of zero, a fullscreen
    /// window, or a build talking to a shell that does not round anything.
    /// The rectangle is the window's own box rather than its surface: a
    /// client with shadows draws well outside the frame, and it is the frame
    /// that has corners.
    pub rounded: Option<(Rectangle<i32, Physical>, i32)>,
    /// The same for `overlay`: the frame the shell drew and the radius on the
    /// *outside* of its border. A border side is a straight band, so the end
    /// of one is the outside of a corner — the desktop's own background, sent
    /// over whatever this window is floating above unless it is cut off here.
    pub overlay_rounded: Option<(Rectangle<i32, Physical>, i32)>,
}

/// What one output should show.
///
/// Worked out without a renderer, so the compositor's state is read once and
/// the backend does not need access to it while its renderer is borrowed.
#[derive(Default)]
pub struct Frame {
    /// Front to back within each group.
    pub layers_above: Vec<(LayerSurface, Point<i32, Physical>)>,
    /// The windows on this output, front to back.
    pub windows: Vec<WindowFrame>,
    pub layers_below: Vec<(LayerSurface, Point<i32, Physical>)>,
    /// The page that runs the desktop. Windows are holes in it, and
    /// `overlay` is cropped out of it.
    pub shell: Option<Shell>,
    /// Other pages, drawn at the same depth as the desktop and each at its own
    /// corner.
    ///
    /// `--url` on a session with more than one monitor is the only thing that
    /// produces one: the page asked for takes the first screen while the
    /// desktop takes the rest. They do not overlap, so their order between
    /// themselves does not matter.
    pub pages: Vec<Shell>,
    /// A terminal drawn as the wallpaper, and where this output starts in it.
    ///
    /// Under the shell rather than over it, which is the only place it can go:
    /// the shell's buffer carries the bar, the taskbar and every window frame,
    /// so anything drawn on top of it hides the desktop. It shows through
    /// because the page stops painting its own background once the compositor
    /// tells it there is something behind — see `Config::background_terminal`.
    ///
    /// One surface across the whole layout, like the shell, which is why this
    /// carries a location: an output at x=2560 shows the part of the terminal
    /// starting there.
    pub background: Option<(WlSurface, Point<i32, Physical>)>,
    /// The part of the shell to draw *above* the windows, in this output's
    /// physical coordinates, and nothing if there is none.
    ///
    /// The shell is one buffer under everything, so this is how anything it
    /// draws can be in front: the same texture, once per rectangle, cropped to
    /// the piece that belongs on top. A notification and a chooser can be up
    /// at the same time, which is why it is a list.
    pub overlay: Vec<(Id, Rectangle<i32, Physical>)>,
    pub cursor: Cursor,
    pub scale: f64,
    /// The lock screen for this output, if the session is locked.
    ///
    /// When present it is the only thing drawn apart from the pointer: the
    /// protocol's guarantee is that nothing else can be seen, and a lock
    /// screen the desktop shows through is not one.
    pub lock: Option<WlSurface>,
    /// Locked with no surface for this output yet — the locker has not drawn,
    /// or has died. Black, because showing the desktop instead would be a way
    /// past the lock.
    pub locked_blank: bool,
}

/// Turn a [`Frame`] into elements, front to back.
///
/// The order is the whole layering policy: pointer, then anything on an
/// overlay or top layer, then the windows, then background and bottom layers,
/// then the shell behind all of it. Hit-testing follows the same order, which
/// is what makes "the click went to what you can see" fall out rather than
/// being computed separately.
pub fn build<R>(frame: &Frame, renderer: &mut R) -> Vec<OutputElement<R>>
where
    R: Renderer + ImportAll + ImportMem + ImportDma,
    // MemoryRenderBufferRenderElement keeps per-renderer textures in a shared
    // map, so the cursor path needs the texture to cross threads even though
    // nothing here does.
    <R as RendererSuper>::TextureId: Clone + Send + Sync + 'static,
{
    let scale = frame.scale;
    let mut elements: Vec<OutputElement<R>> = Vec::new();

    // Locked: the lock surface and the pointer, and nothing else. Returning
    // early is the guarantee — there is no ordering of the desktop that would
    // also be safe.
    if frame.locked_blank || frame.lock.is_some() {
        if let Some(surface) = frame.lock.as_ref() {
            use smithay::backend::renderer::element::surface::render_elements_from_surface_tree;
            elements.extend(
                render_elements_from_surface_tree::<_, WaylandSurfaceRenderElement<R>>(
                    renderer,
                    surface,
                    Point::from((0, 0)),
                    scale,
                    1.0,
                    Kind::Unspecified,
                )
                .into_iter()
                .map(OutputElement::from),
            );
        }
        return elements;
    }

    match &frame.cursor {
        Cursor::Hidden => {}
        Cursor::Surface(surface, hotspot) => {
            use smithay::backend::renderer::element::surface::render_elements_from_surface_tree;
            // Drawn with the hotspot subtracted, so the point the user aims
            // with is where the pointer is.
            let at = Point::from((-hotspot.x, -hotspot.y));
            elements.extend(
                render_elements_from_surface_tree::<_, WaylandSurfaceRenderElement<R>>(
                    renderer,
                    surface,
                    at,
                    scale,
                    1.0,
                    Kind::Cursor,
                )
                .into_iter()
                .map(OutputElement::from),
            );
        }
        Cursor::Image(buffer, at) => {
            if let Ok(element) = MemoryRenderBufferRenderElement::from_buffer(
                renderer,
                at.to_f64(),
                buffer,
                None,
                None,
                None,
                Kind::Cursor,
            ) {
                elements.push(OutputElement::from(element));
            }
        }
    }

    for (layer, location) in &frame.layers_above {
        elements.extend(
            layer
                .render_elements::<WaylandSurfaceRenderElement<R>>(
                    renderer,
                    *location,
                    scale.into(),
                    1.0,
                )
                .into_iter()
                .map(OutputElement::from),
        );
    }

    // A dialog the shell put up, above every window.
    //
    // The same buffer as the copy at the bottom — there is only one — drawn
    // again and cropped to the part that has to be in front. A chooser asking
    // which window to share cannot be behind the windows it is asking about,
    // and the shell has no way to be in front on its own.
    if let Some(shell) = frame.shell.as_ref() {
        for (id, crop) in &frame.overlay {
            let Some(element) = shell_element(renderer, shell, id.clone()) else {
                break;
            };
            // Cropped away to nothing is a notification on another monitor,
            // not a fault.
            if let Some(cropped) = CropRenderElement::from_element(element, scale, *crop) {
                elements.push(OutputElement::from(cropped));
            }
        }
    }

    for WindowFrame {
        window,
        location,
        origin,
        clip,
        scale: window_scale,
        opacity: frame_opacity,
        overlay,
        rounded,
        overlay_rounded,
    } in &frame.windows
    {
        // This window's border, above the windows it is stacked over and below
        // its own surface's popups. The sides only: the middle is the client,
        // and the shell's buffer has the desktop behind it rather than a hole.
        if let Some(shell) = frame.shell.as_ref() {
            for (id, side) in overlay {
                let Some(element) = shell_element(renderer, shell, id.clone()) else {
                    break;
                };
                let Some(cropped) = CropRenderElement::from_element(element, scale, *side) else {
                    continue;
                };
                match overlay_rounded {
                    Some((rect, radius)) => {
                        if let Some(rounded) =
                            RoundedRenderElement::from_element(cropped, scale, *rect, *radius)
                        {
                            elements.push(OutputElement::from(rounded));
                        }
                    }
                    None => elements.push(OutputElement::from(cropped)),
                }
            }
        }

        // Popups first and unclipped, drawn over everything below.
        //
        // A menu overflows the window that opened it — that is what a menu is
        // — and the crop describes the hole the shell drew for the *window*.
        // Cropping the popup to it cuts a Firefox menu off at the window edge,
        // or removes it entirely when it opens past one.
        use smithay::backend::renderer::element::surface::render_elements_from_surface_tree;
        use smithay::desktop::PopupManager;
        use smithay::wayland::seat::WaylandFocus as _;
        if let Some(surface) = window.wl_surface() {
            for (popup, offset) in PopupManager::popups_for_surface(&surface) {
                // The same arithmetic Smithay uses inside
                // `Window::render_elements`, including the window's own
                // geometry offset — a client with shadows draws its window
                // inside a larger surface, and a menu is anchored to the
                // window.
                let at = *location
                    + (window.geometry().loc + offset - popup.geometry().loc)
                        .to_f64()
                        .to_physical(scale)
                        .to_i32_round();
                let drawn = render_elements_from_surface_tree::<_, WaylandSurfaceRenderElement<R>>(
                    renderer,
                    popup.wl_surface(),
                    at,
                    scale,
                    1.0,
                    Kind::Unspecified,
                );
                // Shrunk with the window it belongs to. Uncropped still — a
                // menu is entitled to overflow its window — but a full-size
                // menu over a thumbnail belongs to neither.
                if (*window_scale - 1.0).abs() > f64::EPSILON {
                    elements.extend(drawn.into_iter().map(|element| {
                        OutputElement::from(RescaleRenderElement::from_element(
                            element,
                            *origin,
                            *window_scale,
                        ))
                    }));
                } else {
                    elements.extend(drawn.into_iter().map(OutputElement::from));
                }
            }
        }

        // The window's own surface tree, without its popups: Smithay's
        // `Window::render_elements` draws those as well, and the copy above is
        // the one that is not cropped — leaving both is a menu drawn twice.
        //
        // X11 windows keep the Smithay path, which has no popups of its own to
        // duplicate: an X11 menu is a separate override-redirect window.
        let alpha = frame_opacity.clamp(0.0, 1.0);
        let surfaces = match window.wl_surface() {
            Some(surface) if window.toplevel().is_some() => {
                render_elements_from_surface_tree::<_, WaylandSurfaceRenderElement<R>>(
                    renderer,
                    &surface,
                    *location,
                    scale,
                    alpha,
                    Kind::Unspecified,
                )
            }
            _ => window.render_elements::<WaylandSurfaceRenderElement<R>>(
                renderer,
                *location,
                scale.into(),
                alpha,
            ),
        };

        // Cropped first and scaled after, which is the order the shell's
        // arithmetic assumes: it sends the clip in the window's own unscaled
        // coordinates, because that is the only space the client's buffer
        // means anything in.
        //
        // Scaled about the window's own corner, so a thumbnail shrinks toward
        // where the shell put it rather than toward the origin of the screen.
        let shrink = (*window_scale - 1.0).abs() > f64::EPSILON;

        // Rounded windows take one path for both, because rounding already
        // implies cropping: the corner is cut out of the box the shell drew,
        // and anything of the client outside that box — a shadow, a client
        // half off its column — is not part of a window with corners.
        if let Some((rect, radius)) = rounded {
            let crop = clip.unwrap_or(*rect);
            elements.extend(surfaces.into_iter().filter_map(|surface| {
                let cropped = CropRenderElement::from_element(surface, scale, crop)?;
                let rounded = RoundedRenderElement::from_element(cropped, scale, *rect, *radius)?;
                Some(if shrink {
                    OutputElement::from(RescaleRenderElement::from_element(
                        rounded,
                        *origin,
                        *window_scale,
                    ))
                } else {
                    OutputElement::from(rounded)
                })
            }));
            continue;
        }

        match (clip, shrink) {
            // Cropped to the hole the shell drew. Without this a window
            // mid-animation, or one scrolled half off its column, covers the
            // bar and the wallpaper with its own background.
            (Some(clip), false) => elements.extend(surfaces.into_iter().filter_map(|surface| {
                CropRenderElement::from_element(surface, scale, *clip).map(OutputElement::from)
            })),
            (Some(clip), true) => elements.extend(surfaces.into_iter().filter_map(|surface| {
                CropRenderElement::from_element(surface, scale, *clip).map(|cropped| {
                    OutputElement::from(RescaleRenderElement::from_element(
                        cropped,
                        *origin,
                        *window_scale,
                    ))
                })
            })),
            (None, false) => elements.extend(surfaces.into_iter().map(OutputElement::from)),
            (None, true) => elements.extend(surfaces.into_iter().map(|surface| {
                OutputElement::from(RescaleRenderElement::from_element(
                    surface,
                    *origin,
                    *window_scale,
                ))
            })),
        }
    }

    for (layer, location) in &frame.layers_below {
        elements.extend(
            layer
                .render_elements::<WaylandSurfaceRenderElement<R>>(
                    renderer,
                    *location,
                    scale.into(),
                    1.0,
                )
                .into_iter()
                .map(OutputElement::from),
        );
    }

    // The shell itself, under everything. Every window is a hole in it.
    if let Some(shell) = frame.shell.as_ref() {
        if let Some(element) = shell_element(renderer, shell, shell.id.clone()) {
            elements.push(OutputElement::from(element));
        }
    }
    // And any page beside it — a `--url` page on the first monitor while the
    // desktop has the rest. At the same depth, because they cover different
    // screens; a window over one of them is still a hole in the desktop, which
    // is drawn above this and covers none of the page's screen.
    for page in &frame.pages {
        if let Some(element) = shell_element(renderer, page, page.id.clone()) {
            elements.push(OutputElement::from(element));
        }
    }

    // The wallpaper terminal, under even that.
    //
    // Last in the list is furthest back, so this is the only thing on the
    // screen the shell is in front of — and it is drawn at all only because
    // the shell has been told to leave its own background transparent.
    if let Some((surface, location)) = frame.background.as_ref() {
        use smithay::backend::renderer::element::surface::render_elements_from_surface_tree;
        elements.extend(
            render_elements_from_surface_tree::<_, WaylandSurfaceRenderElement<R>>(
                renderer,
                surface,
                *location,
                scale,
                1.0,
                Kind::Unspecified,
            )
            .into_iter()
            .map(OutputElement::from),
        );
    }

    elements
}

/// The shell's frame as one texture, under the id it is drawn with.
///
/// Imported here rather than held as a texture, because which renderer it
/// belongs to is the backend's business. Renderers cache the import, so this is
/// not a copy per frame — and not per call either, which is what makes drawing
/// it twice affordable.
fn shell_element<R>(
    renderer: &mut R,
    shell: &Shell,
    id: Id,
) -> Option<TextureRenderElement<<R as RendererSuper>::TextureId>>
where
    R: Renderer + ImportAll + ImportMem + ImportDma,
    <R as RendererSuper>::TextureId: Clone + Send + Sync + 'static,
{
    match renderer.import_dmabuf(&shell.buffer, None) {
        Ok(texture) => Some(TextureRenderElement::from_texture_with_damage(
            id,
            renderer.context_id(),
            shell.location,
            texture,
            1,
            Transform::Normal,
            None,
            None,
            None,
            None,
            shell.damage.clone(),
            Kind::Unspecified,
        )),
        Err(e) => {
            // Once in a while, not once a frame.
            //
            // This runs per output per frame, and a buffer that cannot be
            // imported cannot be imported the next time either — so the
            // failure repeats at the refresh rate for as long as it lasts. On
            // a 240Hz screen that is 240 formatted lines a second, and the
            // write to go with them, on the render path. views.rs keeps
            // `last_mismatch` for exactly this reason.
            //
            // Timed rather than a "have I said this" flag, because with more
            // than one GPU the answer differs per renderer: the primary
            // imports the shell fine while a second card cannot, so a plain
            // flag is set and cleared by alternating outputs and every frame
            // prints again.
            static COMPLAINED: std::sync::LazyLock<std::sync::Mutex<Option<std::time::Instant>>> =
                std::sync::LazyLock::new(|| std::sync::Mutex::new(None));
            const EVERY: std::time::Duration = std::time::Duration::from_secs(10);
            let mut last = COMPLAINED.lock().unwrap_or_else(|e| e.into_inner());
            if last.is_none_or(|at| at.elapsed() >= EVERY) {
                *last = Some(std::time::Instant::now());
                // The cause, and what it usually is. Cross-device is the one
                // that happens on a working machine: the shell's buffer is
                // allocated on the GPU the clients are told about, and a
                // screen on another card imports it into that card's own
                // renderer. Vulkan hides it here — every device without a
                // Vulkan driver of its own falls back to the same one, so the
                // import never crosses anything — and the GLES renderer,
                // which binds one EGL context per card, does not.
                tracing::error!(
                    "could not import the shell's frame into this renderer: {e}. \
                     On more than one GPU this is the shell's buffer being \
                     allocated on one card and imported on another; that screen \
                     draws its windows and no desktop behind them."
                );
            }
            None
        }
    }
}

/// The four sides of a border, given the frame the shell drew and the hole
/// inside it.
///
/// The middle is left out on purpose. What the shell paints inside a window is
/// the desktop behind it — `.viewport` has no background of its own, but the
/// wallpaper it sits on does — so a copy of the whole frame drawn over the
/// window is the desktop drawn over the client, which is a floating window
/// turned into a solid rectangle of wallpaper.
///
/// Top and bottom run the full width, so the corners belong to them and the
/// border reads as continuous. A side with no thickness comes back empty and
/// the caller drops it.
pub fn border_sides(
    frame: viewport_ipc::geometry::Box,
    hole: viewport_ipc::geometry::Box,
) -> [viewport_ipc::geometry::Box; 4] {
    use viewport_ipc::geometry::Box;

    let bottom_of_hole = hole.y + hole.height;
    let right_of_hole = hole.x + hole.width;
    [
        // Top.
        Box::new(frame.x, frame.y, frame.width, (hole.y - frame.y).max(0)),
        // Bottom.
        Box::new(
            frame.x,
            bottom_of_hole,
            frame.width,
            (frame.y + frame.height - bottom_of_hole).max(0),
        ),
        // Left, between the two, so the corners are not drawn twice.
        Box::new(frame.x, hole.y, (hole.x - frame.x).max(0), hole.height),
        // Right.
        Box::new(
            right_of_hole,
            hole.y,
            (frame.x + frame.width - right_of_hole).max(0),
            hole.height,
        ),
    ]
}

/// Where a window sits relative to an output, and what it is cropped to.
///
/// Kept here so both backends agree: the geometry origin has to be subtracted,
/// because a client drawing its own decorations puts shadows outside its
/// logical window and `xdg_surface.geometry` marks the real one inside that
/// larger surface.
pub fn window_placement(
    window: &Window,
    layout: Rectangle<i32, Logical>,
    output_geometry: Rectangle<i32, Logical>,
    clip: Option<Rectangle<i32, Logical>>,
    scale: f64,
) -> (Point<i32, Physical>, Option<Rectangle<i32, Physical>>) {
    let location = (layout.loc - output_geometry.loc - window.geometry().loc)
        .to_f64()
        .to_physical(scale)
        .to_i32_round();
    let clip = clip.map(|clip| {
        Rectangle::<i32, Logical>::new(
            (
                clip.loc.x - output_geometry.loc.x,
                clip.loc.y - output_geometry.loc.y,
            )
                .into(),
            clip.size,
        )
        .to_f64()
        .to_physical(scale)
        .to_i32_round()
    });
    (location, clip)
}

#[cfg(test)]
mod tests {
    use super::*;
    use viewport_ipc::geometry::Box;

    /// A frame drawn over the window instead of around it is the desktop drawn
    /// over the client: `.viewport` has no background, but the wallpaper it
    /// sits on does, so the middle of the frame is opaque in the shell's
    /// buffer. That is a floating window turned into a solid rectangle.
    #[test]
    fn the_middle_of_a_border_is_not_drawn() {
        let frame = Box::new(100, 100, 400, 300);
        let hole = Box::new(102, 102, 396, 296);
        let sides = border_sides(frame, hole);

        for side in sides {
            let overlaps_middle = side.x < hole.x + hole.width
                && side.x + side.width > hole.x
                && side.y < hole.y + hole.height
                && side.y + side.height > hole.y;
            assert!(!overlaps_middle, "{side:?} covers the client");
        }
    }

    /// And all four of them are there, each as thick as the border.
    #[test]
    fn every_side_of_a_border_is_drawn() {
        let frame = Box::new(100, 100, 400, 300);
        let hole = Box::new(102, 102, 396, 296);
        let [top, bottom, left, right] = border_sides(frame, hole);

        assert_eq!(top, Box::new(100, 100, 400, 2));
        assert_eq!(bottom, Box::new(100, 398, 400, 2));
        // Between the two, so the corners are drawn once rather than twice.
        assert_eq!(left, Box::new(100, 102, 2, 296));
        assert_eq!(right, Box::new(498, 102, 2, 296));
    }

    /// A window whose frame is its hole has no border, and asking for one
    /// gives four rectangles of nothing rather than four of something.
    #[test]
    fn a_window_with_no_border_gets_no_sides() {
        let box_ = Box::new(0, 0, 800, 600);
        for side in border_sides(box_, box_) {
            assert!(side.width == 0 || side.height == 0, "{side:?} is not empty");
        }
    }
}
