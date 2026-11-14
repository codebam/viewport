// SPDX-License-Identifier: GPL-3.0-or-later
//
// The cross-GPU copy: a buffer the scanout card cannot import, shown anyway.
//
// `multigpu.rs` states the rules — which cards can read a buffer, what the
// default advertisement says, what gets reported once. This module is the
// machinery those rules deferred: the fallback that closes the hole a
// refused buffer would otherwise leave in one screen.
//
// The shape, and why it is this one:
//
//   * The render pass is generic over one renderer (`render_pass` in
//     `udev.rs`, compiled once for Vulkan and once for GLES). A copy needs
//     two renderers live in the same pass — so the pass is handed a wrapper
//     that owns the scanout renderer and borrows the other cards', and
//     delegates everything except the one call that can refuse a buffer.
//     The renderers are moved out of `Udev::devices` for the pass exactly
//     the way the scanout's already was (`Gpu::Placeholder`), because the
//     wrapper cannot borrow them out of `udev` while `render_pass` holds
//     the rest of it. Nothing else touches them meanwhile: recovery runs on
//     the event loop, not mid-pass.
//
//   * The hook is `ImportDmaWl::import_dma_buffer`, which smithay's blanket
//     `ImportAll` dispatches every client dmabuf through. The scanout
//     renderer is asked first, unchanged — the fast path stays fast, and a
//     single-GPU machine never constructs the wrapper at all. Only a
//     refusal starts a copy.
//
//   * The copy imports the buffer on a card that takes it (the helper),
//     draws it into a shadow buffer allocated on the scanout card in a
//     format the scanout card can import (`multigpu::shadow_format`), and
//     hands the scanout renderer the shadow. The shadow is cached for as
//     long as the client keeps the source buffer, so the copy is paid once
//     per buffer, not once per frame — smithay's own per-surface import
//     cache sits on top and asks even less often.
//
//   * If no helper can bind the shadow — two drivers with no renderable
//     format in common — the copy goes through the CPU: read back on the
//     helper, upload on the scanout card. Slower, and the only path that
//     needs no format agreement between the cards at all.
//
//   * Explicit sync is honoured on both sides: the helper waits on the
//     surface's acquire point before it copies (the client's GPU may not
//     have finished writing the source), and smithay's `import_surface`
//     waits on it again before the scanout samples the result. The copy
//     itself is ordered by the shadow's implicit fence: the helper's write
//     travels with the buffer the scanout imports.
//
// What this is not: smithay's own multi-GPU renderer, which is built on its
// GLES `GraphicsApi` where this tree is Vulkan wherever Vulkan works — the
// whole colour-management and explicit-sync path depends on that. The
// wrapper keeps the scanout renderer exactly the renderer it was.
//
// Scope: client buffers, which arrive through `import_dma_buffer`. The
// shell's own buffer is imported directly (`render::shell_element`) and is
// allocated in a modifier the shell's copy renderer and the primary share
// (`shell_copy_modifiers`); a secondary card that refuses even that is a
// separate gap, and the direct import path stays honest about it.

use std::collections::HashMap;
use std::os::unix::io::AsRawFd;
use std::time::Instant;

use smithay::backend::allocator::dmabuf::{Dmabuf, WeakDmabuf};
use smithay::backend::allocator::format::FormatSet;
use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
use smithay::backend::allocator::{Allocator as _, Buffer as _, Fourcc};
use smithay::backend::drm::DrmDeviceFd;
use smithay::backend::renderer::sync::SyncPoint;
use smithay::backend::renderer::{
    Bind, Color32F, ContextId, DebugFlags, ExportMem, Frame, ImportDma, ImportDmaWl, ImportMem,
    ImportMemWl, Offscreen, Renderer, RendererSuper, TextureFilter,
};
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::utils::{Buffer as BufferCoord, Physical, Point, Rectangle, Size, Transform};
use smithay::wayland::compositor::SurfaceData;
use smithay::wayland::drm_syncobj::DrmSyncobjCachedState;

use crate::background_effect::{BackgroundEffectRenderElement, BackgroundEffectRenderer};
use crate::udev::{Captures, ColourSettings, Gpu};

/// How many shadow buffers one session keeps at once.
///
/// A client's swapchain is a handful of buffers; this covers several foreign
/// windows with room to spare, and stops a client that cycles buffers
/// without ever reusing one from growing the cache without bound. Each
/// entry is a full-surface allocation on the scanout card, so the cap is a
/// memory bound, not a politeness. The least recently used goes first.
pub const MAX_COPIED_BUFFERS: usize = 32;

/// The identity of a client buffer, for the cache key.
///
/// A dmabuf is handed over once and shown for as long as the client keeps
/// it; the cache must recognise the same buffer across commits and forget
/// it when it dies. The fds are the identity: `fstat`'s device and inode
/// are stable for the life of the buffer and distinct between buffers,
/// where a pointer to the `Dmabuf` is neither (it is rebuilt from the
/// protocol on every import).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BufferKey(Vec<(u64, u64)>);

impl BufferKey {
    /// The key of this buffer, or `None` if a plane cannot be stat'ed —
    /// a buffer with no readable identity is simply not cached.
    pub fn of(dmabuf: &Dmabuf) -> Option<Self> {
        let mut planes = Vec::new();
        for fd in dmabuf.handles() {
            planes.push(fd_identity(fd)?);
        }
        if planes.is_empty() {
            return None;
        }
        Some(Self(planes))
    }
}

/// `(st_dev, st_ino)` of an fd, the pair that names its file across the
/// system. `fstat` rather than anything on the path: a dmabuf fd has no
/// path, and the numbers are what the kernel keys the buffer by.
fn fd_identity(fd: std::os::unix::io::BorrowedFd<'_>) -> Option<(u64, u64)> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: fstat writes a full `struct stat` on success and nothing on
    // failure; the fd is borrowed and valid for the call.
    let rc = unsafe { libc::fstat(fd.as_raw_fd(), stat.as_mut_ptr()) };
    if rc != 0 {
        return None;
    }
    // SAFETY: rc == 0 means fstat initialised the whole struct.
    let stat = unsafe { stat.assume_init() };
    Some((stat.st_dev, stat.st_ino))
}

/// One cached copy: the shadow on the scanout card, and a weak handle on
/// the source so the entry dies with the client's buffer.
#[derive(Debug)]
struct CopyEntry {
    shadow: Dmabuf,
    source: WeakDmabuf,
    used: Instant,
}

/// The shadows a session has made, keyed by the buffer they copy.
///
/// Lives in `Udev` and is moved out for each render pass, the way the
/// renderers are: the wrapper borrows it while `render_pass` holds the rest
/// of the compositor. `reported` rides along so the once-per-case log of a
/// failed copy survives between passes too.
#[derive(Debug, Default)]
pub struct CopyCache {
    entries: HashMap<BufferKey, CopyEntry>,
    /// Copies that failed, by format, modifier and scanout card — said once,
    /// the same rule `cross_gpu_reported` keeps for the import-time warning.
    pub reported: crate::multigpu::Reported,
}

impl CopyCache {
    /// The shadow for this buffer, if one is cached and its source is alive.
    fn lookup(&mut self, key: &BufferKey) -> Option<&Dmabuf> {
        self.prune();
        let entry = self.entries.get_mut(key)?;
        entry.used = Instant::now();
        Some(&entry.shadow)
    }

    fn insert(&mut self, key: BufferKey, shadow: Dmabuf, source: WeakDmabuf) {
        evict_oldest(&mut self.entries, MAX_COPIED_BUFFERS - 1, |entry| {
            entry.used
        });
        self.entries.insert(
            key,
            CopyEntry {
                shadow,
                source,
                used: Instant::now(),
            },
        );
    }

    fn remove(&mut self, key: &BufferKey) {
        self.entries.remove(key);
    }

    /// Drop the entries whose client buffer is gone. The shadow would be
    /// dropped with them eventually — the cap evicts — but a dead buffer's
    /// shadow is memory serving nothing, and the next import of a reused
    /// key must not find a stale picture.
    fn prune(&mut self) {
        self.entries.retain(|_, entry| !entry.source.is_gone());
    }
}

/// Evict the least recently used entries until the map holds at most `cap`.
///
/// Split out from the cache so the choice is testable without a graphics
/// card: the policy is "oldest `used` goes first", and nothing about it
/// depends on what the entries are.
fn evict_oldest<K: Eq + std::hash::Hash + Clone, V>(
    map: &mut HashMap<K, V>,
    cap: usize,
    used: impl Fn(&V) -> Instant,
) {
    while map.len() > cap {
        let oldest = map
            .iter()
            .min_by_key(|(_, value)| used(value))
            .map(|(key, _)| key.clone());
        match oldest {
            Some(key) => {
                map.remove(&key);
            }
            None => break,
        }
    }
}

/// The scanout renderer, plus the means to copy a buffer it cannot import.
///
/// Every trait the render pass needs is delegated to the scanout renderer
/// unchanged — the element list, the damage tracking, the colour settings,
/// the capture paths all see exactly the renderer they saw before. The one
/// exception is `import_dma_buffer`, where a refusal starts a copy through
/// a helper card. See the module header for why the shape is a wrapper and
/// not a second render pass.
pub struct FallbackRenderer<'a, R> {
    /// The card this output is on. Its renderer draws the frame.
    pub scanout: &'a mut R,
    /// The other online cards' renderers, moved out of `Udev::devices` for
    /// the pass, with the index each came from so it goes back to the right
    /// slot. Any of them may serve as the helper; the first that takes the
    /// buffer does the copy.
    pub helpers: &'a mut [(usize, Gpu)],
    /// The scanout card's GBM device: shadows are allocated here, because
    /// the scanout renderer is the one that has to import them.
    pub gbm: &'a GbmDevice<DrmDeviceFd>,
    /// The session's copies, moved out of `Udev` for the pass.
    pub cache: &'a mut CopyCache,
    /// The scanout card's index, for the once-per-case failure report.
    pub device: usize,
}

impl<R: RendererSuper> std::fmt::Debug for FallbackRenderer<'_, R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FallbackRenderer")
            .field("device", &self.device)
            .field("scanout", &self.scanout)
            .finish_non_exhaustive()
    }
}

impl<R: RendererSuper> RendererSuper for FallbackRenderer<'_, R> {
    type Error = R::Error;
    type TextureId = R::TextureId;
    type Framebuffer<'buffer> = R::Framebuffer<'buffer>;
    type Frame<'frame, 'buffer>
        = FallbackFrame<'frame, 'buffer, R>
    where
        'buffer: 'frame,
        Self: 'frame;
}

impl<R: Renderer> Renderer for FallbackRenderer<'_, R> {
    fn context_id(&self) -> ContextId<Self::TextureId> {
        // The scanout's identity: smithay caches imported textures per
        // surface keyed by this, and the textures are the scanout's.
        self.scanout.context_id()
    }

    fn downscale_filter(&mut self, filter: TextureFilter) -> Result<(), Self::Error> {
        self.scanout.downscale_filter(filter)
    }

    fn upscale_filter(&mut self, filter: TextureFilter) -> Result<(), Self::Error> {
        self.scanout.upscale_filter(filter)
    }

    fn set_debug_flags(&mut self, flags: DebugFlags) {
        self.scanout.set_debug_flags(flags);
    }

    fn debug_flags(&self) -> DebugFlags {
        self.scanout.debug_flags()
    }

    fn render<'frame, 'buffer>(
        &'frame mut self,
        framebuffer: &'frame mut Self::Framebuffer<'buffer>,
        output_size: Size<i32, Physical>,
        dst_transform: Transform,
    ) -> Result<Self::Frame<'frame, 'buffer>, Self::Error>
    where
        'buffer: 'frame,
    {
        self.scanout
            .render(framebuffer, output_size, dst_transform)
            .map(FallbackFrame)
    }

    fn wait(&mut self, sync: &SyncPoint) -> Result<(), Self::Error> {
        self.scanout.wait(sync)
    }

    fn cleanup_texture_cache(&mut self) -> Result<(), Self::Error> {
        self.scanout.cleanup_texture_cache()
    }

    fn invalidate_caches(&mut self) -> Result<(), Self::Error> {
        self.scanout.invalidate_caches()
    }
}

/// One frame of the wrapped renderer. Delegation only — the frame draws
/// with the scanout renderer's textures into the scanout renderer's
/// framebuffer, and nothing about the copy changes that.
pub struct FallbackFrame<'frame, 'buffer, R: RendererSuper>(R::Frame<'frame, 'buffer>)
where
    'buffer: 'frame,
    R: 'frame;

impl<'frame, 'buffer, R: RendererSuper> Frame for FallbackFrame<'frame, 'buffer, R>
where
    'buffer: 'frame,
    R: 'frame,
{
    type Error = R::Error;
    type TextureId = R::TextureId;

    fn context_id(&self) -> ContextId<Self::TextureId> {
        self.0.context_id()
    }

    fn clear(
        &mut self,
        color: Color32F,
        at: &[Rectangle<i32, Physical>],
    ) -> Result<(), Self::Error> {
        self.0.clear(color, at)
    }

    fn draw_solid(
        &mut self,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        color: Color32F,
    ) -> Result<(), Self::Error> {
        self.0.draw_solid(dst, damage, color)
    }

    fn render_texture_from_to(
        &mut self,
        texture: &Self::TextureId,
        src: Rectangle<f64, BufferCoord>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        src_transform: Transform,
        alpha: f32,
    ) -> Result<(), Self::Error> {
        self.0.render_texture_from_to(
            texture,
            src,
            dst,
            damage,
            opaque_regions,
            src_transform,
            alpha,
        )
    }

    fn transformation(&self) -> Transform {
        self.0.transformation()
    }

    fn output_size(&self) -> Size<i32, Physical> {
        self.0.output_size()
    }

    fn wait(&mut self, sync: &SyncPoint) -> Result<(), Self::Error> {
        self.0.wait(sync)
    }

    fn finish(self) -> Result<SyncPoint, Self::Error> {
        self.0.finish()
    }
}

impl<T, R: Bind<T>> Bind<T> for FallbackRenderer<'_, R> {
    fn bind<'a>(&mut self, target: &'a mut T) -> Result<Self::Framebuffer<'a>, Self::Error> {
        self.scanout.bind(target)
    }

    fn supported_formats(&self) -> Option<FormatSet> {
        self.scanout.supported_formats()
    }
}

impl<T, R: Offscreen<T>> Offscreen<T> for FallbackRenderer<'_, R> {
    fn create_buffer(
        &mut self,
        format: Fourcc,
        size: Size<i32, BufferCoord>,
    ) -> Result<T, Self::Error> {
        self.scanout.create_buffer(format, size)
    }
}

impl<R: ImportMem> ImportMem for FallbackRenderer<'_, R> {
    fn import_memory(
        &mut self,
        data: &[u8],
        format: Fourcc,
        size: Size<i32, BufferCoord>,
        flipped: bool,
    ) -> Result<Self::TextureId, Self::Error> {
        self.scanout.import_memory(data, format, size, flipped)
    }

    fn update_memory(
        &mut self,
        texture: &Self::TextureId,
        data: &[u8],
        region: Rectangle<i32, BufferCoord>,
    ) -> Result<(), Self::Error> {
        self.scanout.update_memory(texture, data, region)
    }

    fn mem_formats(&self) -> Box<dyn Iterator<Item = Fourcc>> {
        self.scanout.mem_formats()
    }
}

impl<R: ImportMemWl> ImportMemWl for FallbackRenderer<'_, R> {
    fn import_shm_buffer(
        &mut self,
        buffer: &WlBuffer,
        surface: Option<&SurfaceData>,
        damage: &[Rectangle<i32, BufferCoord>],
    ) -> Result<Self::TextureId, Self::Error> {
        // Shared memory is CPU-written and importable everywhere; there is
        // nothing a second card could add.
        self.scanout.import_shm_buffer(buffer, surface, damage)
    }
}

impl<R: ImportDma> ImportDma for FallbackRenderer<'_, R> {
    fn dmabuf_formats(&self) -> FormatSet {
        self.scanout.dmabuf_formats()
    }

    fn has_dmabuf_format(&self, format: smithay::backend::allocator::Format) -> bool {
        self.scanout.has_dmabuf_format(format)
    }

    fn import_dmabuf(
        &mut self,
        dmabuf: &Dmabuf,
        damage: Option<&[Rectangle<i32, BufferCoord>]>,
    ) -> Result<Self::TextureId, Self::Error> {
        // Direct callers get the scanout's honest answer. The fallback
        // lives in `import_dma_buffer`, the path client buffers take.
        self.scanout.import_dmabuf(dmabuf, damage)
    }
}

impl<R> ImportDmaWl for FallbackRenderer<'_, R>
where
    R: Renderer + ImportDma + ImportMem,
    R::TextureId: Clone + 'static,
{
    fn import_dma_buffer(
        &mut self,
        buffer: &WlBuffer,
        surface: Option<&SurfaceData>,
        damage: &[Rectangle<i32, BufferCoord>],
    ) -> Result<Self::TextureId, Self::Error> {
        let dmabuf = smithay::wayland::dmabuf::get_dmabuf(buffer)
            .expect("import_dma_buffer without checking buffer type?");
        match self.scanout.import_dmabuf(dmabuf, Some(damage)) {
            Ok(texture) => Ok(texture),
            Err(original) => {
                match copied_texture(
                    self.scanout,
                    self.helpers,
                    self.gbm,
                    self.cache,
                    self.device,
                    dmabuf,
                    surface,
                ) {
                    Some(texture) => Ok(texture),
                    // The copy failed too. The original refusal is the
                    // honest error: it names what the scanout card could
                    // not do, and the surface is missing from this screen
                    // exactly as it was before the fallback existed.
                    None => Err(original),
                }
            }
        }
    }
}

impl<R: ExportMem> ExportMem for FallbackRenderer<'_, R> {
    type TextureMapping = R::TextureMapping;

    fn copy_framebuffer(
        &mut self,
        target: &Self::Framebuffer<'_>,
        region: Rectangle<i32, BufferCoord>,
        format: Fourcc,
    ) -> Result<Self::TextureMapping, Self::Error> {
        self.scanout.copy_framebuffer(target, region, format)
    }

    fn copy_texture(
        &mut self,
        texture: &Self::TextureId,
        region: Rectangle<i32, BufferCoord>,
        format: Fourcc,
    ) -> Result<Self::TextureMapping, Self::Error> {
        self.scanout.copy_texture(texture, region, format)
    }

    fn can_read_texture(&mut self, texture: &Self::TextureId) -> Result<bool, Self::Error> {
        self.scanout.can_read_texture(texture)
    }

    fn map_texture<'a>(
        &mut self,
        texture_mapping: &'a Self::TextureMapping,
    ) -> Result<&'a [u8], Self::Error> {
        self.scanout.map_texture(texture_mapping)
    }
}

impl<R: Captures> Captures for FallbackRenderer<'_, R> {
    type Buffer = R::Buffer;

    fn cast_targets(&mut self, size: Size<i32, Physical>) -> Vec<Dmabuf> {
        self.scanout.cast_targets(size)
    }
}

impl<R: ColourSettings> ColourSettings for FallbackRenderer<'_, R> {
    fn set_colour_cube(&mut self, cube: Option<(u64, &[[f32; 4]])>) {
        self.scanout.set_colour_cube(cube);
    }

    fn set_sdr_eotf(&mut self, eotf: viewport_vulkan::color::SdrEotf) {
        self.scanout.set_sdr_eotf(eotf);
    }
}

impl<R: BackgroundEffectRenderer> BackgroundEffectRenderer for FallbackRenderer<'_, R> {
    fn background_effects_available(&self) -> bool {
        self.scanout.background_effects_available()
    }

    fn draw_background_effect<'frame, 'buffer>(
        effect: &BackgroundEffectRenderElement,
        frame: &mut Self::Frame<'frame, 'buffer>,
        src: Rectangle<f64, BufferCoord>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        cache: Option<&smithay::utils::user_data::UserDataMap>,
    ) -> Result<(), Self::Error>
    where
        'buffer: 'frame,
        Self: 'frame,
    {
        R::draw_background_effect(effect, &mut frame.0, src, dst, damage, cache)
    }

    fn capture_background_effect<'frame, 'buffer>(
        effect: &BackgroundEffectRenderElement,
        frame: &mut Self::Frame<'frame, 'buffer>,
        src: Rectangle<f64, BufferCoord>,
        dst: Rectangle<i32, Physical>,
        cache: &smithay::utils::user_data::UserDataMap,
    ) -> Result<(), Self::Error>
    where
        'buffer: 'frame,
        Self: 'frame,
    {
        R::capture_background_effect(effect, &mut frame.0, src, dst, cache)
    }
}

/// A texture for a buffer the scanout card refused, or `None`.
///
/// The cache first: a shadow made for an earlier commit of the same buffer
/// is still the picture, and re-importing it is what the scanout renderer's
/// own import cache makes cheap. A cached shadow that no longer imports —
/// the card was reset and rebuilt under it — is dropped and copied again.
///
/// Then the helpers, in seat order. The GPU path is preferred on every
/// helper before the CPU path is tried on any: a copy that stays on the
/// cards is worth more than one that crosses the CPU, even if it means
/// asking a second helper.
fn copied_texture<R>(
    scanout: &mut R,
    helpers: &mut [(usize, Gpu)],
    gbm: &GbmDevice<DrmDeviceFd>,
    cache: &mut CopyCache,
    device: usize,
    dmabuf: &Dmabuf,
    surface: Option<&SurfaceData>,
) -> Option<R::TextureId>
where
    R: Renderer + ImportDma + ImportMem,
    R::TextureId: Clone + 'static,
{
    let size = dmabuf.size();
    let format = dmabuf.format();
    let key = BufferKey::of(dmabuf);

    if let Some(key) = key.as_ref() {
        if let Some(shadow) = cache.lookup(key) {
            match scanout.import_dmabuf(shadow, None) {
                Ok(texture) => return Some(texture),
                Err(_) => cache.remove(key),
            }
        }
    }

    // The client's acquire point, if it named one: its GPU may not have
    // finished writing the buffer. The helper waits before copying;
    // smithay's `import_surface` waits again before the scanout samples
    // the result, so both ends of the copy are fenced.
    let acquire = surface.and_then(|states| {
        states
            .cached_state
            .get::<DrmSyncobjCachedState>()
            .current()
            .acquire_point
            .clone()
    });

    let shadow_format = crate::multigpu::shadow_format(&scanout.dmabuf_formats());
    let mut allocator = GbmAllocator::new(gbm.clone(), GbmBufferFlags::RENDERING);
    let physical: Size<i32, Physical> = (size.w, size.h).into();

    // The GPU path: import on the helper, draw into a shadow on the scanout
    // card, import the shadow. The body is a closure because `with_gpu!`
    // compiles it for both helper renderers and an early `return` would
    // leave the whole function, not just this helper's attempt.
    for (helper_index, gpu) in helpers.iter_mut() {
        let copied: Option<R::TextureId> = crate::with_gpu!(gpu, |helper| (|| {
            use smithay::backend::allocator::dmabuf::AsDmabuf as _;
            if let Some(acquire) = acquire.as_ref() {
                let _ = helper.wait(&SyncPoint::from(acquire.clone()));
            }
            let texture = helper.import_dmabuf(dmabuf, None).ok()?;
            let format = shadow_format?;
            let mut shadow = allocator
                .create_buffer(
                    size.w as u32,
                    size.h as u32,
                    format.code,
                    &[format.modifier],
                )
                .ok()?
                .export()
                .ok()?;
            crate::dump::copy_texture(helper, &texture, &mut shadow, physical).ok()?;
            let texture = scanout.import_dmabuf(&shadow, None).ok()?;
            if let Some(key) = key.clone() {
                cache.insert(key, shadow, dmabuf.weak());
            }
            tracing::debug!(
                "cross-GPU copy: gpu {helper_index} drew a ({:?}, modifier {:#x}) buffer for gpu {device}",
                format.code,
                u64::from(format.modifier),
            );
            Some(texture)
        })());
        if copied.is_some() {
            return copied;
        }
    }

    // The CPU path: no format agreement between the cards is needed at all.
    // Not cached as a shadow — there is no buffer to re-import — but
    // smithay's per-surface import cache keeps it to once per commit, the
    // same rhythm every other import keeps.
    for (helper_index, gpu) in helpers.iter_mut() {
        let copied: Option<R::TextureId> = crate::with_gpu!(gpu, |helper| (|| {
            if let Some(acquire) = acquire.as_ref() {
                let _ = helper.wait(&SyncPoint::from(acquire.clone()));
            }
            let texture = helper.import_dmabuf(dmabuf, None).ok()?;
            let bytes = read_back(helper, &texture, size)?;
            let texture = scanout
                .import_memory(&bytes, Fourcc::Argb8888, size, false)
                .ok()?;
            tracing::debug!(
                "cross-GPU copy (CPU): gpu {helper_index} read a ({:?}, modifier {:#x}) buffer back for gpu {device}",
                format.code,
                u64::from(format.modifier),
            );
            Some(texture)
        })());
        if copied.is_some() {
            return copied;
        }
    }

    if cache
        .reported
        .first_time(format.code as u32, format.modifier.into(), device)
    {
        tracing::warn!(
            "a client buffer ({:?}, modifier {:#x}) imports on no helper card either, \
             or no copy path between the cards works; it stays missing from gpu {device}'s \
             screens. Set cross_gpu = \"portable\" to advertise only what every card can \
             read — see docs/configuration.md.",
            format.code,
            u64::from(format.modifier),
        );
    }
    None
}

/// Read a texture back through the helper's own offscreen, as ARGB8888.
///
/// The same shape `dump::output_frame` uses: a texture imported from a
/// client buffer is not necessarily readable directly (it was created
/// without TRANSFER_SRC), so it is drawn into an offscreen the helper
/// allocated and that is read back instead.
fn read_back<H>(
    helper: &mut H,
    texture: &H::TextureId,
    size: Size<i32, BufferCoord>,
) -> Option<Vec<u8>>
where
    H: Renderer
        + Captures
        + Bind<<H as Captures>::Buffer>
        + Offscreen<<H as Captures>::Buffer>
        + ExportMem,
{
    let physical: Size<i32, Physical> = (size.w, size.h).into();
    let mut target = helper.create_buffer(Fourcc::Argb8888, size).ok()?;
    let mut framebuffer = helper.bind(&mut target).ok()?;
    let mut frame = helper
        .render(&mut framebuffer, physical, Transform::Normal)
        .ok()?;
    frame
        .render_texture_at(
            texture,
            Point::from((0, 0)),
            1,
            1.0,
            Transform::Normal,
            &[Rectangle::from_size(physical)],
            &[],
            1.0,
        )
        .ok()?;
    let _ = frame.finish().ok()?;
    let mapping = helper
        .copy_framebuffer(&framebuffer, Rectangle::from_size(size), Fourcc::Argb8888)
        .ok()?;
    Some(helper.map_texture(&mapping).ok()?.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::io::AsFd;

    #[test]
    fn a_buffer_key_is_the_same_for_the_same_fds_and_different_for_others() {
        // Socket pairs stand in for dmabuf plane fds: fstat names them the
        // same way the kernel names a buffer's memory, and two pairs are
        // two identities.
        let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
        let (c, d) = std::os::unix::net::UnixStream::pair().unwrap();
        let key_a = fd_identity(a.as_fd()).unwrap();
        assert_eq!(fd_identity(a.as_fd()).unwrap(), key_a);
        assert_ne!(fd_identity(b.as_fd()).unwrap(), key_a);
        assert_ne!(fd_identity(c.as_fd()).unwrap(), key_a);
        drop(d);
        // Closing one end of a pair leaves the other's identity intact.
        assert!(fd_identity(b.as_fd()).is_some());
    }

    #[test]
    fn a_dmabuf_key_covers_every_plane() {
        use smithay::backend::allocator::dmabuf::DmabufFlags;
        use smithay::backend::allocator::Modifier;

        let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
        let (c, d) = std::os::unix::net::UnixStream::pair().unwrap();
        let mut builder = Dmabuf::builder(
            (64, 64),
            Fourcc::Argb8888,
            Modifier::Linear,
            DmabufFlags::empty(),
        );
        assert!(builder.add_plane(a.as_fd().try_clone_to_owned().unwrap(), 0, 256));
        assert!(builder.add_plane(c.as_fd().try_clone_to_owned().unwrap(), 0, 256));
        let dmabuf = builder.build().unwrap();
        let key = BufferKey::of(&dmabuf).unwrap();
        assert_eq!(key.0.len(), 2);
        assert_eq!(key.0[0], fd_identity(a.as_fd()).unwrap());
        assert_eq!(key.0[1], fd_identity(c.as_fd()).unwrap());
        // Rebuilt from the same fds — the protocol rebuilds a Dmabuf on
        // every import — the key is unchanged.
        let mut builder = Dmabuf::builder(
            (64, 64),
            Fourcc::Argb8888,
            Modifier::Linear,
            DmabufFlags::empty(),
        );
        assert!(builder.add_plane(a.as_fd().try_clone_to_owned().unwrap(), 0, 256));
        assert!(builder.add_plane(c.as_fd().try_clone_to_owned().unwrap(), 0, 256));
        assert_eq!(BufferKey::of(&builder.build().unwrap()), Some(key));
        drop(b);
        drop(d);
    }

    #[test]
    fn eviction_takes_the_least_recently_used_first() {
        let base = Instant::now();
        let mut map: HashMap<u32, Instant> = HashMap::new();
        map.insert(1, base);
        map.insert(2, base + std::time::Duration::from_secs(1));
        map.insert(3, base + std::time::Duration::from_secs(2));
        evict_oldest(&mut map, 2, |used| *used);
        assert_eq!(map.len(), 2);
        assert!(!map.contains_key(&1));
        assert!(map.contains_key(&2));
        assert!(map.contains_key(&3));
        // Under the cap nothing goes, and an empty map is left alone.
        evict_oldest(&mut map, 5, |used| *used);
        assert_eq!(map.len(), 2);
        let mut empty: HashMap<u32, Instant> = HashMap::new();
        evict_oldest(&mut empty, 0, |used| *used);
        assert!(empty.is_empty());
    }

    #[test]
    fn the_cache_holds_at_most_the_cap() {
        let mut cache = CopyCache::default();
        let (a, _b) = std::os::unix::net::UnixStream::pair().unwrap();
        for n in 0..(MAX_COPIED_BUFFERS as u32 + 8) {
            // Distinct keys without distinct buffers: the eviction policy
            // is what is under test, not the identity.
            let key = BufferKey(vec![(0, u64::from(n))]);
            let mut builder = Dmabuf::builder(
                (4, 4),
                Fourcc::Argb8888,
                smithay::backend::allocator::Modifier::Linear,
                smithay::backend::allocator::dmabuf::DmabufFlags::empty(),
            );
            assert!(builder.add_plane(a.as_fd().try_clone_to_owned().unwrap(), 0, 16));
            let shadow = builder.build().unwrap();
            cache.insert(key, shadow.clone(), shadow.weak());
        }
        assert!(cache.entries.len() <= MAX_COPIED_BUFFERS);
        // The newest entries are the ones that survived.
        assert!(cache
            .entries
            .contains_key(&BufferKey(vec![(0, MAX_COPIED_BUFFERS as u64 + 7)])));
    }

    #[test]
    fn the_fallback_asks_the_scanout_first_and_only_on_refusal() {
        // Source invariant: the fast path is the scanout's own import, and
        // the copy is the error branch — never the other way round, which
        // would put a cross-card copy under every buffer on every machine.
        let source = include_str!("cross_gpu.rs");
        let source = source.split("#[cfg(test)]").next().unwrap_or(source);
        let start = source
            .find("fn import_dma_buffer(")
            .expect("the import hook");
        let end = start
            + source[start..]
                .find("impl<R: ExportMem>")
                .expect("the next impl");
        let body = &source[start..end];
        let ask = body
            .find("self.scanout.import_dmabuf(dmabuf, Some(damage))")
            .expect("the scanout is asked");
        let copy = body.find("copied_texture(").expect("the copy");
        assert!(ask < copy, "the scanout must be asked before any copy");
        assert!(
            body.contains("Err(original)"),
            "the copy must hang off the refusal, not the success"
        );
    }

    #[test]
    fn the_pass_restores_every_renderer_it_took() {
        // Source invariant: the helpers and the cache go back exactly the
        // way the scanout renderer always has — a Placeholder left behind
        // is a panic on the next frame.
        let source = include_str!("udev.rs");
        let start = source
            .find("let mut helpers: Vec<(usize, Gpu)>")
            .expect("the helper extraction");
        let end = start
            + source[start..]
                .find("self.udev = Some(udev);")
                .expect("the restore");
        let body = &source[start..end];
        assert!(
            body.contains("for (index, gpu) in helpers"),
            "every helper goes back to its slot"
        );
        assert!(
            body.contains("device.renderer = gpu;"),
            "the scanout renderer goes back"
        );
        assert!(
            body.contains("udev.cross_gpu_copies = copies;"),
            "the cache goes back"
        );
    }
}
