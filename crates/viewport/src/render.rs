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
use smithay::backend::renderer::element::utils::CropRenderElement;
use smithay::backend::renderer::element::{AsRenderElements as _, Id, Kind};
use smithay::backend::renderer::utils::DamageSnapshot;
use smithay::backend::renderer::{ImportAll, ImportDma, ImportMem, Renderer, RendererSuper};
use smithay::desktop::{LayerSurface, Window};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Buffer as BufferCoord, Logical, Physical, Point, Rectangle, Transform};

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
    Shell=TextureRenderElement<<R as RendererSuper>::TextureId>,
    Cursor=MemoryRenderBufferRenderElement<R>,
}

/// The pointer image, resolved but not yet imported.
pub enum Cursor {
    /// Nothing to draw: a client asked for the pointer to be hidden, or it is
    /// on another output.
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

/// What one output should show.
///
/// Worked out without a renderer, so the compositor's state is read once and
/// the backend does not need access to it while its renderer is borrowed.
#[derive(Default)]
pub struct Frame {
    /// Front to back within each group.
    pub layers_above: Vec<(LayerSurface, Point<i32, Physical>)>,
    /// Window, where to draw it, and the hole it is cropped to.
    pub windows: Vec<(Window, Point<i32, Physical>, Option<Rectangle<i32, Physical>>)>,
    pub layers_below: Vec<(LayerSurface, Point<i32, Physical>)>,
    pub shell: Option<Shell>,
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

impl Default for Cursor {
    fn default() -> Self {
        Self::Hidden
    }
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

    for (window, location, clip) in &frame.windows {
        let surfaces = window.render_elements::<WaylandSurfaceRenderElement<R>>(
            renderer,
            *location,
            scale.into(),
            1.0,
        );
        match clip {
            // Cropped to the hole the shell drew. Without this a window
            // mid-animation, or one scrolled half off its column, covers the
            // bar and the wallpaper with its own background.
            Some(clip) => elements.extend(surfaces.into_iter().filter_map(|surface| {
                CropRenderElement::from_element(surface, scale, *clip).map(OutputElement::from)
            })),
            None => elements.extend(surfaces.into_iter().map(OutputElement::from)),
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

    if let Some(shell) = frame.shell.as_ref() {
        // Imported here rather than held as a texture, because which renderer
        // it belongs to is the backend's business. Renderers cache the import,
        // so this is not a copy per frame.
        match renderer.import_dmabuf(&shell.buffer, None) {
            Ok(texture) => elements.push(OutputElement::from(
                TextureRenderElement::from_texture_with_damage(
                    shell.id.clone(),
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
                ),
            )),
            Err(_) => tracing::error!("could not import the shell's frame into this renderer"),
        }
    }

    elements
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
            (clip.loc.x - output_geometry.loc.x, clip.loc.y - output_geometry.loc.y).into(),
            clip.size,
        )
        .to_f64()
        .to_physical(scale)
        .to_i32_round()
    });
    (location, clip)
}
