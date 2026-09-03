// SPDX-License-Identifier: GPL-3.0-or-later
//
// ext-background-effect-v1, and the GLES framebuffer effect that makes its
// blur capability true. The global is deliberately created by the backend,
// after this renderer has proved it can compile and copy what the effect uses.

use std::cell::RefCell;
use std::sync::{Arc, Mutex};

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::{Element, Id, RenderElement};
use smithay::backend::renderer::gles::{
    Capability as GlesCapability, GlesError, GlesFrame, GlesRenderer, GlesTexProgram, GlesTexture,
    Uniform, UniformName, UniformType,
};
use smithay::backend::renderer::utils::{CommitCounter, DamageSet};
use smithay::backend::renderer::{
    Bind, BlitFrame, Frame as _, FrameContext, ImportAll, Offscreen, Renderer, Texture as _,
    TextureFilter,
};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Logical, Physical, Point, Rectangle, Scale, Size};
use smithay::wayland::background_effect::{
    BackgroundEffectState, BackgroundEffectSurfaceCachedState, Capability,
    ExtBackgroundEffectHandler,
};
use smithay::wayland::compositor::{
    add_post_commit_hook, with_states, RectangleKind, RegionAttributes, SurfaceData,
};

use crate::state::ViewportState;

const BLUR_SHADER: &str = r#"#version 100

//_DEFINES_

#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
#endif

precision highp float;
#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif

uniform float alpha;
uniform vec2 texel_size;
varying vec2 v_coords;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

vec4 sample_at(vec2 offset) {
    vec2 edge = texel_size * 0.5;
    return texture2D(tex, clamp(v_coords + offset, edge, vec2(1.0) - edge));
}

void main() {
    vec2 radius = texel_size * 4.0;
    vec4 color = sample_at(vec2(0.0)) * 0.204164;
    color += sample_at(vec2( radius.x, 0.0)) * 0.123841;
    color += sample_at(vec2(-radius.x, 0.0)) * 0.123841;
    color += sample_at(vec2(0.0,  radius.y)) * 0.123841;
    color += sample_at(vec2(0.0, -radius.y)) * 0.123841;
    color += sample_at(vec2( radius.x,  radius.y)) * 0.075114;
    color += sample_at(vec2(-radius.x,  radius.y)) * 0.075114;
    color += sample_at(vec2( radius.x, -radius.y)) * 0.075114;
    color += sample_at(vec2(-radius.x, -radius.y)) * 0.075114;

#if defined(NO_ALPHA)
    color = vec4(color.rgb, 1.0) * alpha;
#else
    color *= alpha;
#endif

#if defined(DEBUG_FLAGS)
    if (tint == 1.0)
        color = vec4(0.0, 0.2, 0.0, 0.2) + color * 0.8;
#endif

    gl_FragColor = color;
}
"#;

const BLUR_DOWNSAMPLE: i32 = 4;
// BLUR_SHADER samples four downsampled texels from its centre sample.
const BLUR_SOURCE_PADDING: i32 = BLUR_DOWNSAMPLE * 4;
const MAX_REGION_OPERATIONS: usize = 256;
const MAX_RESOLVED_RECTS: usize = 1024;
const MAX_BLUR_TEXTURE_DIMENSION: i32 = 4096;
const MAX_BLUR_TEXTURE_PIXELS: i64 = 4 * 1024 * 1024;
const MAX_BACKGROUND_EFFECTS_PER_FRAME: usize = 64;

/// GPU work admitted into one independently composited image.
#[derive(Debug)]
pub(crate) struct BackgroundEffectBudget {
    remaining_effects: usize,
    remaining_texture_pixels: i64,
}

impl Default for BackgroundEffectBudget {
    fn default() -> Self {
        Self {
            remaining_effects: MAX_BACKGROUND_EFFECTS_PER_FRAME,
            remaining_texture_pixels: MAX_BLUR_TEXTURE_PIXELS,
        }
    }
}

impl BackgroundEffectBudget {
    fn take(&mut self, source_size: Size<i32, Physical>) -> bool {
        let Some((_, pixels)) = blur_texture_size(source_size.w, source_size.h) else {
            return false;
        };
        if self.remaining_effects == 0 || pixels > self.remaining_texture_pixels {
            return false;
        }
        self.remaining_effects -= 1;
        self.remaining_texture_pixels -= pixels;
        true
    }
}

#[derive(Debug, Clone)]
struct BlurProgram(GlesTexProgram);

#[derive(Debug)]
struct SurfaceEffectData(Mutex<SurfaceEffect>);

#[derive(Debug)]
struct SurfaceEffect {
    id: Id,
    commit: CommitCounter,
    hook_registered: bool,
    forced: bool,
    rects: Option<Arc<Vec<Rectangle<i32, Logical>>>>,
}

impl SurfaceEffect {
    fn register_hook(&mut self) -> bool {
        if self.hook_registered {
            false
        } else {
            self.hook_registered = true;
            true
        }
    }

    fn set_committed(&mut self, rects: Option<Arc<Vec<Rectangle<i32, Logical>>>>) -> bool {
        if self.rects == rects {
            return false;
        }
        self.rects = rects;
        self.commit.increment();
        true
    }

    fn set_forced(&mut self, forced: bool) {
        if self.forced != forced {
            self.forced = forced;
            self.commit.increment();
        }
    }
}

impl Default for SurfaceEffectData {
    fn default() -> Self {
        Self(Mutex::new(SurfaceEffect {
            id: Id::new(),
            commit: CommitCounter::default(),
            hook_registered: false,
            forced: false,
            rects: None,
        }))
    }
}

/// One requested blur, already placed in this output's physical coordinates.
#[derive(Debug)]
pub struct BackgroundEffectRenderElement {
    id: Id,
    commit: CommitCounter,
    geometry: Rectangle<i32, Physical>,
    alpha: f32,
    /// Requested pixels, relative to `geometry` and numerically in pixels.
    regions: Vec<Rectangle<i32, Buffer>>,
}

impl Element for BackgroundEffectRenderElement {
    fn id(&self) -> &Id {
        &self.id
    }

    fn current_commit(&self) -> CommitCounter {
        self.commit
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        Rectangle::from_size((self.geometry.size.w as f64, self.geometry.size.h as f64).into())
    }

    fn geometry(&self, _scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.geometry
    }

    fn damage_since(
        &self,
        _scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        if commit == Some(self.commit) {
            return DamageSet::default();
        }
        // The previous region is not available here. Damage the bounding box
        // so pixels removed by a subtraction are repainted from the backdrop.
        DamageSet::from_slice(&[Rectangle::from_size(self.geometry.size)])
    }

    fn alpha(&self) -> f32 {
        self.alpha
    }

    fn is_framebuffer_effect(&self) -> bool {
        true
    }
}

/// Renderer half of the framebuffer effect.
///
/// Vulkan implements this as unavailable: its compositor never advertises the
/// protocol, so no element can be requested there. Keeping the no-op impl lets
/// the renderer-neutral element list stay one type for both DRM renderers.
pub trait BackgroundEffectRenderer: Renderer {
    fn background_effects_available(&self) -> bool {
        false
    }

    fn draw_background_effect(
        _effect: &BackgroundEffectRenderElement,
        _frame: &mut Self::Frame<'_, '_>,
        _src: Rectangle<f64, Buffer>,
        _dst: Rectangle<i32, Physical>,
        _damage: &[Rectangle<i32, Physical>],
        _cache: Option<&UserDataMap>,
    ) -> Result<(), Self::Error> {
        Ok(())
    }

    fn capture_background_effect(
        _effect: &BackgroundEffectRenderElement,
        _frame: &mut Self::Frame<'_, '_>,
        _src: Rectangle<f64, Buffer>,
        _dst: Rectangle<i32, Physical>,
        _cache: &UserDataMap,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

impl BackgroundEffectRenderer for viewport_vulkan::VulkanRenderer {}

impl BackgroundEffectRenderer for GlesRenderer {
    fn background_effects_available(&self) -> bool {
        self.egl_context()
            .user_data()
            .get::<BlurProgram>()
            .is_some()
    }

    fn draw_background_effect(
        effect: &BackgroundEffectRenderElement,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        effect.draw_gles(frame, src, dst, damage, cache)
    }

    fn capture_background_effect(
        effect: &BackgroundEffectRenderElement,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        cache: &UserDataMap,
    ) -> Result<(), GlesError> {
        effect.capture_gles(frame, src, dst, cache)
    }
}

impl<R> RenderElement<R> for BackgroundEffectRenderElement
where
    R: BackgroundEffectRenderer,
{
    fn draw(
        &self,
        frame: &mut R::Frame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), R::Error> {
        R::draw_background_effect(self, frame, src, dst, damage, cache)
    }

    fn capture_framebuffer(
        &self,
        frame: &mut R::Frame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        cache: &UserDataMap,
    ) -> Result<(), R::Error> {
        R::capture_background_effect(self, frame, src, dst, cache)
    }
}

#[derive(Debug, Default)]
struct GlesEffectCache {
    texture: Option<GlesTexture>,
    captured: Option<CapturedFramebuffer>,
    damage: Vec<Rectangle<i32, Physical>>,
}

#[derive(Debug, Clone, Copy)]
struct CapturedFramebuffer {
    dst: Rectangle<i32, Physical>,
    transform: smithay::utils::Transform,
}

impl BackgroundEffectRenderElement {
    #[cfg(test)]
    pub(crate) fn for_test(requested: Rectangle<i32, Physical>) -> Self {
        let geometry = expand_blur_geometry(requested);
        Self {
            id: Id::new(),
            commit: CommitCounter::default(),
            geometry,
            alpha: 1.0,
            regions: vec![Rectangle::new(
                (
                    requested.loc.x - geometry.loc.x,
                    requested.loc.y - geometry.loc.y,
                )
                    .into(),
                (requested.size.w, requested.size.h).into(),
            )],
        }
    }

    /// Restrict pixels drawn by a layout crop without discarding backdrop
    /// source padding around them.
    pub(crate) fn clip_regions(&mut self, clip: Rectangle<i32, Physical>) -> bool {
        self.regions = self
            .regions
            .iter()
            .filter_map(|region| {
                let absolute = Rectangle::<i32, Physical>::new(
                    self.geometry.loc + Point::from((region.loc.x, region.loc.y)),
                    (region.size.w, region.size.h).into(),
                );
                let clipped = absolute.intersection(clip)?;
                Some(Rectangle::<i32, Buffer>::new(
                    (
                        clipped.loc.x - self.geometry.loc.x,
                        clipped.loc.y - self.geometry.loc.y,
                    )
                        .into(),
                    (clipped.size.w, clipped.size.h).into(),
                ))
            })
            .collect();
        !self.regions.is_empty()
    }

    fn capture_gles(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        cache: &UserDataMap,
    ) -> Result<(), GlesError> {
        let output = Rectangle::from_size(frame.output_size());
        let Some(clamped) = dst.intersection(output) else {
            return Ok(());
        };
        let transformed = frame
            .transformation()
            .transform_rect_in(clamped, &output.size);
        // Keep scratch storage at the pre-wrapper source resolution. Overview
        // animations then scale the blur instead of reallocating it every frame.
        let clamp_scale = clamped.size.to_f64() / dst.size.to_f64();
        let source_size = Size::<i32, Buffer>::from((
            (src.size.w * clamp_scale.x).round().max(1.0) as i32,
            (src.size.h * clamp_scale.y).round().max(1.0) as i32,
        ));
        let source_size = frame.transformation().transform_size(source_size);
        let cache = cache.get_or_insert::<RefCell<GlesEffectCache>, _>(Default::default);
        let mut cache = cache.borrow_mut();
        let Some((size, _)) = blur_texture_size(source_size.w, source_size.h) else {
            cache.texture = None;
            cache.captured = None;
            return Ok(());
        };
        if cache
            .texture
            .as_ref()
            .is_some_and(|texture| texture.size() != size)
        {
            cache.texture = None;
            cache.captured = None;
        }
        if cache.texture.is_none() {
            let mut renderer = frame.renderer();
            cache.texture = Some(renderer.as_mut().create_buffer(Fourcc::Abgr8888, size)?);
        }

        let texture = cache.texture.as_mut().expect("created above");
        let mut renderer = frame.renderer();
        let mut target = renderer.as_mut().bind(texture)?;
        drop(renderer);
        let sync = frame.blit_to(
            &mut target,
            transformed,
            Rectangle::from_size((size.w, size.h).into()),
            TextureFilter::Linear,
        )?;
        frame.wait(&sync)?;
        drop(target);
        cache.captured = Some(CapturedFramebuffer {
            dst: clamped,
            transform: frame.transformation(),
        });
        Ok(())
    }

    fn draw_gles(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        let Some(cache) = cache.and_then(|cache| cache.get::<RefCell<GlesEffectCache>>()) else {
            return Ok(());
        };
        let output = Rectangle::from_size(frame.output_size());

        let mut cache = cache.borrow_mut();
        let Some(texture) = cache.texture.clone() else {
            return Ok(());
        };
        let Some(captured) = cache.captured else {
            return Ok(());
        };
        let Some(clamped) = dst
            .intersection(output)
            .and_then(|draw| draw.intersection(captured.dst))
        else {
            return Ok(());
        };
        let clamp = Rectangle::new(clamped.loc - dst.loc, clamped.size);
        cache.damage.clear();
        for region in &self.regions {
            let Some(mapped) = map_region(*region, src, dst.size) else {
                continue;
            };
            for damaged in damage {
                let Some(mut draw) = mapped
                    .intersection(*damaged)
                    .and_then(|draw| draw.intersection(clamp))
                else {
                    continue;
                };
                draw.loc -= clamp.loc;
                cache.damage.push(draw);
            }
        }
        if cache.damage.is_empty() {
            return Ok(());
        }

        let program = {
            let renderer = frame.renderer();
            renderer
                .as_ref()
                .egl_context()
                .user_data()
                .get::<BlurProgram>()
                .cloned()
        };
        let Some(program) = program else {
            return Ok(());
        };
        let size = texture.size();
        let uniforms = [Uniform::new(
            "texel_size",
            [1.0 / size.w.max(1) as f32, 1.0 / size.h.max(1) as f32],
        )];
        let texture_src = captured_source(captured, clamped, texture.size());
        frame.render_texture_from_to(
            &texture,
            texture_src,
            clamped,
            &cache.damage,
            &[],
            captured.transform.invert(),
            self.alpha,
            Some(&program.0),
            &uniforms,
        )
    }
}

/// The captured texture is laid out in framebuffer-transform coordinates.
/// Pick the matching part when a wrapper draws the effect as rounded bands.
fn captured_source(
    captured: CapturedFramebuffer,
    drawn: Rectangle<i32, Physical>,
    texture_size: Size<i32, Buffer>,
) -> Rectangle<f64, Buffer> {
    let relative = Rectangle::new(drawn.loc - captured.dst.loc, drawn.size);
    let source = captured
        .transform
        .transform_rect_in(relative, &captured.dst.size);
    let captured_size = captured.transform.transform_size(captured.dst.size);
    let scale_x = f64::from(texture_size.w) / f64::from(captured_size.w);
    let scale_y = f64::from(texture_size.h) / f64::from(captured_size.h);
    Rectangle::new(
        (
            f64::from(source.loc.x) * scale_x,
            f64::from(source.loc.y) * scale_y,
        )
            .into(),
        (
            f64::from(source.size.w) * scale_x,
            f64::from(source.size.h) * scale_y,
        )
            .into(),
    )
}

/// Map one region from the element's original source into draw-relative pixels.
fn map_region(
    region: Rectangle<i32, Buffer>,
    src: Rectangle<f64, Buffer>,
    dst_size: smithay::utils::Size<i32, Physical>,
) -> Option<Rectangle<i32, Physical>> {
    let region = region.to_f64().intersection(src)?;
    if src.size.w <= 0.0 || src.size.h <= 0.0 {
        return None;
    }
    let scale_x = f64::from(dst_size.w) / src.size.w;
    let scale_y = f64::from(dst_size.h) / src.size.h;
    let x1 = ((region.loc.x - src.loc.x) * scale_x).round() as i32;
    let y1 = ((region.loc.y - src.loc.y) * scale_y).round() as i32;
    let x2 = ((region.loc.x + region.size.w - src.loc.x) * scale_x).round() as i32;
    let y2 = ((region.loc.y + region.size.h - src.loc.y) * scale_y).round() as i32;
    let rect = Rectangle::new((x1, y1).into(), (x2 - x1, y2 - y1).into());
    (!rect.is_empty()).then_some(rect)
}

/// Build the effect paired with one imported Wayland surface element.
pub(crate) fn render_element<R>(
    states: &SurfaceData,
    surface: &WaylandSurfaceRenderElement<R>,
    scale: Scale<f64>,
    force_blur: bool,
    budget: &mut BackgroundEffectBudget,
) -> Option<BackgroundEffectRenderElement>
where
    R: Renderer + ImportAll,
{
    let data = if force_blur {
        states
            .data_map
            .get_or_insert_threadsafe(SurfaceEffectData::default)
    } else {
        states.data_map.get::<SurfaceEffectData>()?
    };
    let (id, commit, rects) = {
        let mut data = data.0.lock().unwrap_or_else(|e| e.into_inner());
        data.set_forced(force_blur);
        (data.id.clone(), data.commit, data.rects.clone())
    };

    let view = surface.view();
    let surface_geometry = surface.geometry(scale);
    let forced;
    let rects = if force_blur {
        forced = vec![Rectangle::from_size(view.dst)];
        &forced
    } else {
        rects.as_deref()?
    };
    let physical: Vec<_> = rects
        .iter()
        .filter_map(|rect| map_surface_region(*rect, view.dst, surface_geometry))
        .filter(|rect| !rect.is_empty())
        .collect();
    let requested_geometry = physical
        .iter()
        .copied()
        .reduce(|left, right| left.merge(right))?;
    let geometry = expand_blur_geometry(requested_geometry);
    if !budget.take(geometry.size) {
        return None;
    }
    let regions: Vec<Rectangle<i32, Buffer>> = physical
        .into_iter()
        .map(|rect| {
            Rectangle::<i32, Buffer>::new(
                (rect.loc.x - geometry.loc.x, rect.loc.y - geometry.loc.y).into(),
                (rect.size.w, rect.size.h).into(),
            )
        })
        .collect();

    Some(BackgroundEffectRenderElement {
        id,
        commit,
        geometry,
        alpha: surface.alpha(),
        regions,
    })
}

pub(crate) fn expand_blur_geometry(geometry: Rectangle<i32, Physical>) -> Rectangle<i32, Physical> {
    let x1 = geometry.loc.x.saturating_sub(BLUR_SOURCE_PADDING);
    let y1 = geometry.loc.y.saturating_sub(BLUR_SOURCE_PADDING);
    let x2 = geometry
        .loc
        .x
        .saturating_add(geometry.size.w)
        .saturating_add(BLUR_SOURCE_PADDING);
    let y2 = geometry
        .loc
        .y
        .saturating_add(geometry.size.h)
        .saturating_add(BLUR_SOURCE_PADDING);
    Rectangle::new(
        (x1, y1).into(),
        (x2.saturating_sub(x1), y2.saturating_sub(y1)).into(),
    )
}

fn blur_texture_size(width: i32, height: i32) -> Option<(Size<i32, Buffer>, i64)> {
    if width <= 0 || height <= 0 {
        return None;
    }
    let downsample = i64::from(BLUR_DOWNSAMPLE);
    let width = (i64::from(width) + downsample - 1) / downsample;
    let height = (i64::from(height) + downsample - 1) / downsample;
    if width > i64::from(MAX_BLUR_TEXTURE_DIMENSION)
        || height > i64::from(MAX_BLUR_TEXTURE_DIMENSION)
    {
        return None;
    }
    let pixels = width.checked_mul(height)?;
    if pixels > MAX_BLUR_TEXTURE_PIXELS {
        return None;
    }
    let size = Size::from((i32::try_from(width).ok()?, i32::try_from(height).ok()?));
    Some((size, pixels))
}

fn map_surface_region(
    region: Rectangle<i32, Logical>,
    surface_size: smithay::utils::Size<i32, Logical>,
    surface_geometry: Rectangle<i32, Physical>,
) -> Option<Rectangle<i32, Physical>> {
    if surface_size.w <= 0 || surface_size.h <= 0 {
        return None;
    }
    let region = region.intersection(Rectangle::from_size(surface_size))?;
    let scale_x = f64::from(surface_geometry.size.w) / f64::from(surface_size.w);
    let scale_y = f64::from(surface_geometry.size.h) / f64::from(surface_size.h);
    let x1 = (f64::from(region.loc.x) * scale_x).round() as i32;
    let y1 = (f64::from(region.loc.y) * scale_y).round() as i32;
    let x2 = (f64::from(region.loc.x + region.size.w) * scale_x).round() as i32;
    let y2 = (f64::from(region.loc.y + region.size.h) * scale_y).round() as i32;
    Rectangle::new(
        surface_geometry.loc + Point::<i32, Physical>::from((x1, y1)),
        (x2 - x1, y2 - y1).into(),
    )
    .intersection(surface_geometry)
}

fn committed_region(states: &SurfaceData) -> Option<RegionAttributes> {
    if !states
        .cached_state
        .has::<BackgroundEffectSurfaceCachedState>()
    {
        return None;
    }
    let mut cached = states
        .cached_state
        .get::<BackgroundEffectSurfaceCachedState>();
    let current = cached.current();
    if current
        .blur_region
        .as_ref()
        .is_some_and(|region| region.rects.len() > MAX_REGION_OPERATIONS)
    {
        current.blur_region = None;
    }
    current.blur_region.clone()
}

/// Resolve ordered region add/subtract operations into disjoint rectangles.
fn resolve_region(region: &RegionAttributes) -> Vec<Rectangle<i32, Logical>> {
    if region.rects.len() > MAX_REGION_OPERATIONS {
        return Vec::new();
    }
    let mut resolved = Vec::new();
    for (kind, rect) in &region.rects {
        if rect.is_empty() {
            continue;
        }
        match kind {
            RectangleKind::Add => {
                let mut added = vec![*rect];
                for existing in &resolved {
                    added = added
                        .into_iter()
                        .flat_map(|part| subtract(part, *existing))
                        .collect();
                    if added.len() > MAX_RESOLVED_RECTS {
                        return Vec::new();
                    }
                }
                resolved.extend(added);
            }
            RectangleKind::Subtract => {
                resolved = resolved
                    .into_iter()
                    .flat_map(|part| subtract(part, *rect))
                    .collect();
            }
        }
        if resolved.len() > MAX_RESOLVED_RECTS {
            return Vec::new();
        }
    }
    resolved
}

fn subtract(
    from: Rectangle<i32, Logical>,
    cut: Rectangle<i32, Logical>,
) -> Vec<Rectangle<i32, Logical>> {
    from.subtract_rect(cut)
}

fn ensure_commit_hook_for<D, F>(surface: &WlSurface, on_changed: F)
where
    D: 'static,
    F: Fn(&mut D, &WlSurface) + Send + Sync + 'static,
{
    let register_hook = with_states(surface, |states| {
        let data = states
            .data_map
            .get_or_insert_threadsafe(SurfaceEffectData::default);
        let mut data = data.0.lock().unwrap_or_else(|e| e.into_inner());
        data.register_hook()
    });

    if register_hook {
        add_post_commit_hook::<D, _>(surface, move |state, _, surface| {
            let changed = with_states(surface, |states| {
                let rects =
                    committed_region(states).map(|region| Arc::new(resolve_region(&region)));
                let Some(data) = states.data_map.get::<SurfaceEffectData>() else {
                    return false;
                };
                let mut data = data.0.lock().unwrap_or_else(|e| e.into_inner());
                data.set_committed(rects)
            });
            if changed {
                on_changed(state, surface);
            }
        });
    }
}

fn ensure_commit_hook(surface: &WlSurface) {
    ensure_commit_hook_for::<ViewportState, _>(surface, |state, surface| {
        state.mark_dirty_for_surface(surface);
    });
}

fn reject_oversized_pending_region(surface: &WlSurface, region: &RegionAttributes) -> bool {
    if region.rects.len() <= MAX_REGION_OPERATIONS {
        return false;
    }
    with_states(surface, |states| {
        states
            .cached_state
            .get::<BackgroundEffectSurfaceCachedState>()
            .pending()
            .blur_region = None;
    });
    true
}

impl ExtBackgroundEffectHandler for ViewportState {
    fn capabilities(&self) -> Capability {
        Capability::Blur
    }

    fn set_blur_region(&mut self, surface: WlSurface, region: RegionAttributes) {
        ensure_commit_hook(&surface);
        if reject_oversized_pending_region(&surface, &region) {
            tracing::debug!(
                "ext-background-effect-v1: ignored a region with more than {MAX_REGION_OPERATIONS} operations"
            );
        }
    }

    fn unset_blur_region(&mut self, surface: WlSurface) {
        ensure_commit_hook(&surface);
    }
}

impl ViewportState {
    /// Publish the protocol only after this GLES context can execute its blur.
    pub fn advertise_background_effects(
        &mut self,
        renderer: &mut GlesRenderer,
    ) -> anyhow::Result<()> {
        if self.background_effect_state.is_some() {
            return Ok(());
        }
        anyhow::ensure!(
            renderer.capabilities().contains(&GlesCapability::Blit),
            "OpenGL ES 3 framebuffer blits are unavailable"
        );
        if renderer
            .egl_context()
            .user_data()
            .get::<BlurProgram>()
            .is_none()
        {
            let program = renderer
                .compile_custom_texture_shader(
                    BLUR_SHADER,
                    &[UniformName::new("texel_size", UniformType::_2f)],
                )
                .map_err(|e| anyhow::anyhow!("compiling the blur shader: {e}"))?;
            renderer
                .egl_context()
                .user_data()
                .insert_if_missing_threadsafe(|| BlurProgram(program));
        }
        self.background_effect_state =
            Some(BackgroundEffectState::new::<Self>(&self.display_handle));
        tracing::info!("ext-background-effect-v1: blur available through GLES");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, width: i32, height: i32) -> Rectangle<i32, Logical> {
        Rectangle::new((x, y).into(), (width, height).into())
    }

    fn area(rects: &[Rectangle<i32, Logical>]) -> i32 {
        rects.iter().map(|rect| rect.size.w * rect.size.h).sum()
    }

    #[test]
    fn ordered_region_subtracts_and_readds() {
        let region = RegionAttributes {
            rects: vec![
                (RectangleKind::Add, rect(0, 0, 10, 10)),
                (RectangleKind::Subtract, rect(2, 2, 6, 6)),
                (RectangleKind::Add, rect(4, 4, 2, 2)),
            ],
        };
        let resolved = resolve_region(&region);

        assert_eq!(area(&resolved), 68);
        assert!(resolved.iter().any(|part| part.contains((4, 4))));
        assert!(!resolved.iter().any(|part| part.contains((3, 3))));
        for (at, left) in resolved.iter().enumerate() {
            for right in &resolved[at + 1..] {
                assert!(!left.overlaps(*right), "{left:?} overlaps {right:?}");
            }
        }
    }

    #[test]
    fn overlapping_adds_remain_one_region() {
        let region = RegionAttributes {
            rects: vec![
                (RectangleKind::Add, rect(0, 0, 10, 10)),
                (RectangleKind::Add, rect(5, 0, 10, 10)),
            ],
        };
        assert_eq!(area(&resolve_region(&region)), 150);
    }

    #[test]
    fn protocol_regions_stop_at_operation_and_fragment_limits() {
        let too_many_operations = RegionAttributes {
            rects: (0..=MAX_REGION_OPERATIONS)
                .map(|x| (RectangleKind::Add, rect(x as i32, 0, 1, 1)))
                .collect(),
        };
        assert!(resolve_region(&too_many_operations).is_empty());

        let mut fragmented = RegionAttributes {
            rects: vec![(RectangleKind::Add, rect(0, 0, 256, 256))],
        };
        for x in (1..128).step_by(2) {
            fragmented
                .rects
                .push((RectangleKind::Subtract, rect(x, 0, 1, 256)));
        }
        for y in (1..128).step_by(2) {
            fragmented
                .rects
                .push((RectangleKind::Subtract, rect(0, y, 256, 1)));
        }
        assert!(fragmented.rects.len() <= MAX_REGION_OPERATIONS);
        assert!(resolve_region(&fragmented).is_empty());
    }

    #[test]
    fn extreme_region_coordinates_do_not_overflow() {
        let pieces = subtract(rect(1, 1, i32::MAX, i32::MAX), rect(2, 2, 1, 1));
        assert!(!pieces.is_empty());
    }

    #[test]
    fn cropped_regions_follow_rescaled_elements() {
        let mapped = map_region(
            Rectangle::new((20, 10).into(), (40, 20).into()),
            Rectangle::new((10.0, 0.0).into(), (80.0, 40.0).into()),
            (40, 20).into(),
        )
        .unwrap();
        assert_eq!(mapped, Rectangle::new((5, 5).into(), (20, 10).into()));
    }

    #[test]
    fn a_subsurface_region_uses_its_own_rounded_geometry() {
        let geometry = Rectangle::new((201, 151).into(), (151, 121).into());
        let full = map_surface_region(rect(0, 0, 100, 80), (100, 80).into(), geometry).unwrap();
        let corner = map_surface_region(rect(0, 0, 20, 10), (100, 80).into(), geometry).unwrap();

        assert_eq!(full, geometry);
        assert_eq!(corner.loc, geometry.loc);
        assert_eq!(corner.size, (30, 15).into());
    }

    #[test]
    fn blur_geometry_keeps_kernel_source_outside_the_draw_mask() {
        let geometry = Rectangle::new((20, 30).into(), (100, 80).into());
        assert_eq!(
            expand_blur_geometry(geometry),
            Rectangle::new((4, 14).into(), (132, 112).into())
        );
    }

    #[test]
    fn layout_crop_keeps_blur_padding_but_clips_draw_regions() {
        let mut effect = BackgroundEffectRenderElement {
            id: Id::new(),
            commit: CommitCounter::default(),
            geometry: Rectangle::new((4, 14).into(), (132, 112).into()),
            alpha: 1.0,
            regions: vec![Rectangle::new((16, 16).into(), (100, 80).into())],
        };
        assert!(effect.clip_regions(Rectangle::new((20, 30).into(), (50, 80).into())));
        assert_eq!(
            effect.geometry,
            Rectangle::new((4, 14).into(), (132, 112).into())
        );
        assert_eq!(
            effect.regions,
            vec![Rectangle::new((16, 16).into(), (50, 80).into())]
        );

        assert!(!effect.clip_regions(Rectangle::new((200, 200).into(), (10, 10).into())));
    }

    #[test]
    fn blur_texture_dimensions_round_up_without_overflow() {
        assert_eq!(blur_texture_size(1, 1), Some(((1, 1).into(), 1)));
        assert_eq!(blur_texture_size(5, 9), Some(((2, 3).into(), 6)));
        assert!(blur_texture_size(0, 10).is_none());
        assert!(blur_texture_size(i32::MAX, i32::MAX).is_none());
    }

    #[test]
    fn frame_budget_bounds_effect_count_and_total_texture_storage() {
        let mut count = BackgroundEffectBudget::default();
        for _ in 0..MAX_BACKGROUND_EFFECTS_PER_FRAME {
            assert!(count.take((1, 1).into()));
        }
        assert!(!count.take((1, 1).into()));

        let mut pixels = BackgroundEffectBudget::default();
        for _ in 0..4 {
            assert!(pixels.take((4096, 4096).into()));
        }
        assert!(!pixels.take((1, 1).into()));

        let mut rejected = BackgroundEffectBudget::default();
        assert!(!rejected.take((i32::MAX, i32::MAX).into()));
        assert!(rejected.take((1, 1).into()));
    }

    #[test]
    fn a_region_change_repaints_removed_pixels() {
        let old_commit = CommitCounter::default();
        let mut commit = old_commit;
        commit.increment();
        let effect = BackgroundEffectRenderElement {
            id: Id::new(),
            commit,
            geometry: Rectangle::new((20, 30).into(), (100, 80).into()),
            alpha: 1.0,
            regions: vec![Rectangle::new((0, 0).into(), (10, 10).into())],
        };

        let damage: Vec<_> = effect
            .damage_since(Scale::from(1.0), Some(old_commit))
            .into_iter()
            .collect();
        assert_eq!(damage, vec![Rectangle::from_size((100, 80).into())]);
    }

    #[test]
    fn committed_region_changes_advance_damage_once() {
        let state = SurfaceEffectData::default();
        let mut state = state.0.lock().unwrap();
        let before = state.commit;

        assert!(state.register_hook(), "first request installs the hook");
        assert!(!state.register_hook(), "the existing hook is reused");

        let first = Some(Arc::new(vec![rect(0, 0, 20, 10)]));
        assert!(state.set_committed(first.clone()));
        assert_ne!(state.commit, before);
        let first_commit = state.commit;
        assert!(
            !state.set_committed(first),
            "re-reading one commit does not create new damage"
        );
        assert_eq!(state.commit, first_commit);

        assert!(state.set_committed(Some(Arc::new(vec![rect(10, 0, 20, 10)]))));
        assert_ne!(state.commit, first_commit);
        assert!(state.set_committed(None));
        assert!(state.rects.is_none());
    }

    #[test]
    fn rounded_pieces_select_their_transformed_capture_pixels() {
        let captured = CapturedFramebuffer {
            dst: Rectangle::new((10, 20).into(), (100, 80).into()),
            transform: smithay::utils::Transform::Flipped180,
        };
        let source = captured_source(
            captured,
            Rectangle::new((30, 30).into(), (20, 10).into()),
            (25, 20).into(),
        );

        assert_eq!(
            source,
            Rectangle::new((5.0, 15.0).into(), (5.0, 2.5).into())
        );
    }
}

#[cfg(test)]
#[path = "background_effect_transaction_tests.rs"]
mod transaction_tests;
