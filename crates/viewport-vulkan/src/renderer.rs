// SPDX-License-Identifier: MIT
//
// Smithay's renderer traits, over the Vulkan pieces in the rest of this crate.
//
// This is the layer that lets `smithay::desktop::space::render_output` drive a
// Vulkan renderer without knowing it is one.
//
// One shape difference is worth explaining. Smithay's `Frame` hands textures
// to the renderer *during* the frame, but Vulkan forbids layout transitions
// and queue ownership transfers inside a render pass — so a texture first seen
// mid-frame could not legally be acquired. The resolution is to acquire at
// import time instead: `import_dmabuf` submits the barrier that takes the
// image from the foreign queue and leaves it in SHADER_READ_ONLY_OPTIMAL, so
// by the time a frame draws with it there is nothing left to transition. That
// is also why the import cache matters for correctness of cost rather than
// just speed: without it every frame would re-import and re-submit.

use ash::vk;
use smithay::backend::allocator::dmabuf::{Dmabuf, WeakDmabuf};
use smithay::backend::allocator::{Format, Fourcc};
use smithay::backend::renderer::sync::SyncPoint;
use smithay::backend::renderer::{
    Bind, Color32F, ContextId, DebugFlags, ExportMem, Frame, ImportDma, ImportMem, Renderer,
    RendererSuper, Texture, TextureFilter,
};
use smithay::utils::{Buffer as BufferCoord, Physical, Rectangle, Size, Transform};

use crate::command::Commands;
use crate::image::{Image, Purpose};
use crate::pipeline::Pipelines;
use crate::{format, Device};

/// Anything that can go wrong in the renderer.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Vulkan(#[from] vk::Result),

    /// A rectangle, format or transform this renderer cannot express.
    #[error("{0}")]
    Unsupported(String),

    #[error("{0:#}")]
    Other(#[from] anyhow::Error),
}

/// A client buffer, imported and ready to sample.
///
/// Cloning is cheap and shares the underlying image: Smithay hands textures
/// around by value and expects that to be free.
#[derive(Debug, Clone)]
pub struct VulkanTexture {
    image: std::sync::Arc<Image>,
    /// The buffer's y axis runs the other way. An shm client may say so, and
    /// the flip is applied when sampling rather than by copying the rows in a
    /// different order.
    flipped: bool,
    /// Only images this renderer allocated can be uploaded into. A DMA-BUF is
    /// the client's memory and writing to it would be a data race with the
    /// client, not an optimisation.
    uploadable: bool,
    /// What the pixels in this buffer mean. Defaults to SDR sRGB, which is
    /// what a client that says nothing is assumed to have sent.
    description: crate::color::Description,
}

impl VulkanTexture {
    pub fn image(&self) -> &Image {
        &self.image
    }

    pub fn is_flipped(&self) -> bool {
        self.flipped
    }

    pub fn description(&self) -> &crate::color::Description {
        &self.description
    }

    /// Say what this buffer's pixels mean.
    ///
    /// Taken by value because a texture is a cheap handle: the caller gets a
    /// second view of the same image with a different description, which is
    /// what a client changing its image description mid-stream produces.
    pub fn with_description(mut self, description: crate::color::Description) -> Self {
        self.description = description;
        self
    }
}

impl Texture for VulkanTexture {
    fn width(&self) -> u32 {
        self.image.width()
    }

    fn height(&self) -> u32 {
        self.image.height()
    }

    fn format(&self) -> Option<Fourcc> {
        Some(self.image.fourcc())
    }
}

/// A render target, borrowed from the `Dmabuf` it was bound from.
#[derive(Debug)]
pub struct VulkanFramebuffer<'buffer> {
    image: std::sync::Arc<Image>,
    _borrow: std::marker::PhantomData<&'buffer mut Dmabuf>,
}

impl Texture for VulkanFramebuffer<'_> {
    fn width(&self) -> u32 {
        self.image.width()
    }

    fn height(&self) -> u32 {
        self.image.height()
    }

    fn format(&self) -> Option<Fourcc> {
        Some(self.image.fourcc())
    }
}

/// Something that can allocate a DMA-BUF.
///
/// A trait of its own rather than Smithay's `Allocator` because that one has
/// associated types, which cannot be made into a trait object — and the point
/// here is for the renderer to hold *an* allocator without being generic over
/// which, or gaining a GBM device it does not otherwise need.
pub trait DmabufAllocator {
    fn allocate(
        &mut self,
        width: u32,
        height: u32,
        format: Fourcc,
        modifiers: &[smithay::backend::allocator::Modifier],
    ) -> anyhow::Result<Dmabuf>;
}

impl<A> DmabufAllocator for smithay::backend::allocator::gbm::GbmAllocator<A>
where
    // The same bound Smithay puts on its own Allocator impl for GbmAllocator.
    A: std::os::fd::AsFd + 'static,
{
    fn allocate(
        &mut self,
        width: u32,
        height: u32,
        format: Fourcc,
        modifiers: &[smithay::backend::allocator::Modifier],
    ) -> anyhow::Result<Dmabuf> {
        use smithay::backend::allocator::dmabuf::AsDmabuf;
        use smithay::backend::allocator::Allocator as _;

        let buffer = self
            .create_buffer(width, height, format, modifiers)
            .map_err(|e| anyhow::anyhow!("gbm allocation: {e}"))?;
        buffer
            .export()
            .map_err(|e| anyhow::anyhow!("exporting the gbm buffer: {e}"))
    }
}

/// A Vulkan renderer for Smithay.
pub struct VulkanRenderer {
    device: Device,
    commands: Commands,
    pipelines: Pipelines,
    context_id: ContextId<VulkanTexture>,
    debug_flags: DebugFlags,

    /// Imported client buffers, so a surface is not re-imported every frame.
    ///
    /// A `Vec` rather than a map because `WeakDmabuf` is compared by identity
    /// and the list is short — a compositor has as many entries here as it has
    /// mapped surfaces.
    imported: Vec<(WeakDmabuf, VulkanTexture)>,

    /// Render targets, keyed the same way. Binding the same output buffer each
    /// frame is the common case, and re-importing it would mean a
    /// vkCreateImage and a memory allocation per frame.
    targets: Vec<(WeakDmabuf, std::sync::Arc<Image>)>,

    /// Optional, because a renderer does not need one: buffers normally
    /// arrive from clients or from the compositor's own swapchain. It is
    /// required only for `Offscreen`, which has to create its own targets.
    allocator: Option<Box<dyn DmabufAllocator>>,

    /// Textures for shm `wl_buffer`s, so a surface that commits every frame is
    /// updated in place rather than reallocated. Keyed by object id; see
    /// `VulkanRenderer::forget_shm_buffer`.
    #[cfg(feature = "wayland")]
    pub(crate) shm: crate::wayland::ShmCache,

    /// What the output expects. Every textured draw converts into this.
    output: crate::color::Description,
}

impl std::fmt::Debug for VulkanRenderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VulkanRenderer")
            .field("device", &self.device.name())
            .field("imported", &self.imported.len())
            .finish()
    }
}

impl VulkanRenderer {
    pub fn new(device: &Device) -> Result<Self, Error> {
        Ok(Self {
            device: device.clone(),
            commands: Commands::new(device)?,
            pipelines: Pipelines::new(device)?,
            context_id: ContextId::default(),
            debug_flags: DebugFlags::empty(),
            imported: Vec::new(),
            targets: Vec::new(),
            allocator: None,
            #[cfg(feature = "wayland")]
            shm: Vec::new(),
            output: crate::color::Description::default(),
        })
    }

    /// A renderer that can also allocate its own render targets.
    ///
    /// Only needed for [`smithay::backend::renderer::Offscreen`]; everything
    /// else takes the buffers it is given.
    pub fn with_allocator(
        device: &Device,
        allocator: impl DmabufAllocator + 'static,
    ) -> Result<Self, Error> {
        let mut renderer = Self::new(device)?;
        renderer.allocator = Some(Box::new(allocator));
        Ok(renderer)
    }

    /// What the output expects; textures are converted into it.
    pub fn output_description(&self) -> &crate::color::Description {
        &self.output
    }

    /// Set what the output expects.
    ///
    /// An HDR output would be PQ with BT.2020 primaries; an ordinary one is
    /// the sRGB default. Everything drawn is converted into this, which is
    /// what lets an SDR and an HDR surface share a screen without one of them
    /// being wrong.
    pub fn set_output_description(&mut self, description: crate::color::Description) {
        self.output = description;
    }

    pub fn can_allocate(&self) -> bool {
        self.allocator.is_some()
    }

    pub fn device(&self) -> &Device {
        &self.device
    }

    /// Drop cache entries whose buffer is gone.
    fn reap(&mut self) {
        self.imported.retain(|(weak, _)| !weak.is_gone());
        self.targets.retain(|(weak, _)| !weak.is_gone());
    }

    /// Wait for a sync point, on the GPU where possible.
    ///
    /// An exportable fence becomes a semaphore the next submission waits on,
    /// so nothing blocks here. A sync point that cannot be exported — one
    /// backed by something other than a fence fd — leaves no choice but to
    /// wait on the CPU.
    fn wait_for(&mut self, sync: &SyncPoint) -> Result<(), Error> {
        if sync.is_reached() {
            return Ok(());
        }
        if let Some(fd) = sync.export() {
            return self.commands.wait_on(fd).map_err(Error::from);
        }
        sync.wait()
            .map_err(|_| Error::Unsupported("interrupted waiting for a sync point".to_owned()))
    }

    /// Take an imported image from the foreign queue, once.
    ///
    /// Submitted on its own rather than folded into a frame, because this has
    /// to happen outside any render pass.
    fn acquire_now(&mut self, image: &Image, layout: vk::ImageLayout) -> Result<(), Error> {
        let barrier = image.acquire_barrier(layout);
        let buffer = self.commands.begin()?;
        unsafe {
            self.device.handle().cmd_pipeline_barrier(
                buffer,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::FRAGMENT_SHADER
                    | vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[barrier],
            );
        }
        self.commands.submit()?;
        Ok(())
    }
}

impl RendererSuper for VulkanRenderer {
    type Error = Error;
    type TextureId = VulkanTexture;
    type Framebuffer<'buffer> = VulkanFramebuffer<'buffer>;
    type Frame<'frame, 'buffer>
        = VulkanFrame<'frame, 'buffer>
    where
        'buffer: 'frame,
        Self: 'frame;
}

impl Renderer for VulkanRenderer {
    fn context_id(&self) -> ContextId<Self::TextureId> {
        self.context_id.clone()
    }

    fn downscale_filter(&mut self, _filter: TextureFilter) -> Result<(), Self::Error> {
        // The sampler is created linear and is not swapped per draw. Changing
        // it would mean a second sampler and a second descriptor push; nothing
        // asks for nearest yet.
        Ok(())
    }

    fn upscale_filter(&mut self, _filter: TextureFilter) -> Result<(), Self::Error> {
        Ok(())
    }

    fn set_debug_flags(&mut self, flags: DebugFlags) {
        self.debug_flags = flags;
    }

    fn debug_flags(&self) -> DebugFlags {
        self.debug_flags
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
        VulkanFrame::begin(self, framebuffer, output_size, dst_transform)
    }

    fn wait(&mut self, sync: &SyncPoint) -> Result<(), Self::Error> {
        self.wait_for(sync)
    }

    fn cleanup_texture_cache(&mut self) -> Result<(), Self::Error> {
        self.reap();
        Ok(())
    }
}

impl ImportDma for VulkanRenderer {
    fn dmabuf_formats(&self) -> smithay::backend::allocator::format::FormatSet {
        format::importable(self.device.physical(), format::COMMON_FORMATS)
            .into_iter()
            .collect()
    }

    fn has_dmabuf_format(&self, format: Format) -> bool {
        format::modifiers(self.device.physical(), format.code)
            .into_iter()
            .any(|s| s.modifier == format.modifier && s.sampling && s.planes == 1)
    }

    fn import_dmabuf(
        &mut self,
        dmabuf: &Dmabuf,
        _damage: Option<&[Rectangle<i32, BufferCoord>]>,
    ) -> Result<Self::TextureId, Self::Error> {
        self.reap();
        if let Some((_, texture)) = self
            .imported
            .iter()
            .find(|(weak, _)| weak.upgrade().as_ref() == Some(dmabuf))
        {
            // Already imported and already acquired. The client may have
            // painted into it since, but the image and its memory are the
            // same; re-importing would allocate a second one over the same fd.
            return Ok(texture.clone());
        }

        let image = Image::import(&self.device, dmabuf, Purpose::Sample)?;
        self.acquire_now(&image, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)?;

        let texture = VulkanTexture {
            image: std::sync::Arc::new(image),
            flipped: false,
            uploadable: false,
            description: crate::color::Description::default(),
        };
        self.imported.push((dmabuf.weak(), texture.clone()));
        Ok(texture)
    }
}

impl ImportMem for VulkanRenderer {
    fn import_memory(
        &mut self,
        data: &[u8],
        format: Fourcc,
        size: Size<i32, BufferCoord>,
        flipped: bool,
    ) -> Result<Self::TextureId, Self::Error> {
        let (width, height) = (size.w.max(0) as u32, size.h.max(0) as u32);
        if width == 0 || height == 0 {
            return Err(Error::Unsupported("a zero-sized memory import".to_owned()));
        }

        let image = Image::allocate(&self.device, width, height, format)?;
        let texture = VulkanTexture {
            image: std::sync::Arc::new(image),
            flipped,
            uploadable: true,
            description: crate::color::Description::default(),
        };

        let whole = Rectangle::from_size(size);
        self.upload(&texture, data, whole, vk::ImageLayout::UNDEFINED)?;
        Ok(texture)
    }

    fn update_memory(
        &mut self,
        texture: &Self::TextureId,
        data: &[u8],
        region: Rectangle<i32, BufferCoord>,
    ) -> Result<(), Self::Error> {
        if !texture.uploadable {
            // A DMA-BUF is the client's memory. Writing into it would be a
            // data race with the client rather than an update.
            return Err(Error::Unsupported(
                "this texture was imported from a dmabuf and cannot be uploaded into".to_owned(),
            ));
        }
        // Already in SHADER_READ_ONLY_OPTIMAL from the previous upload, and
        // its contents outside the region have to survive.
        self.upload(
            texture,
            data,
            region,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        )
    }

    fn mem_formats(&self) -> Box<dyn Iterator<Item = Fourcc>> {
        // The four shm formats every Wayland compositor is expected to take.
        // Argb8888 and Xrgb8888 are the two wl_shm guarantees.
        Box::new(
            [
                Fourcc::Argb8888,
                Fourcc::Xrgb8888,
                Fourcc::Abgr8888,
                Fourcc::Xbgr8888,
            ]
            .into_iter(),
        )
    }
}

impl VulkanRenderer {
    /// Copy CPU pixels into an image this renderer owns.
    ///
    /// Waits for the copy before returning. The staging buffer is freed here,
    /// so the alternative is keeping it alive until a fence signals — worth
    /// doing when shm turns out to be hot, and not before.
    fn upload(
        &mut self,
        texture: &VulkanTexture,
        data: &[u8],
        region: Rectangle<i32, BufferCoord>,
        from_layout: vk::ImageLayout,
    ) -> Result<(), Error> {
        let image = &texture.image;
        let (x, y) = (region.loc.x.max(0), region.loc.y.max(0));
        let (w, h) = (region.size.w.max(0) as u32, region.size.h.max(0) as u32);
        if w == 0 || h == 0 {
            return Err(Error::Unsupported("an empty upload region".to_owned()));
        }
        if x as u32 + w > image.width() || y as u32 + h > image.height() {
            return Err(Error::Unsupported(format!(
                "region {region:?} is outside the {}x{} texture",
                image.width(),
                image.height()
            )));
        }

        // Four bytes per pixel: every format in `mem_formats` is 32bpp.
        let stride = image.width() as usize * 4;
        let needed = stride * image.height() as usize;
        if data.len() < needed {
            // The trait says too small is an error and beyond is truncated.
            return Err(Error::Unsupported(format!(
                "{} bytes of pixels for a {}x{} texture that needs {needed}",
                data.len(),
                image.width(),
                image.height()
            )));
        }

        // The whole buffer is staged even for a partial update, so the copy
        // can use the source stride directly instead of packing rows.
        let mut staging = crate::staging::Staging::new(&self.device, needed as vk::DeviceSize)?;
        staging.write(0, &data[..needed])?;

        let buffer = self.commands.begin()?;
        let handle = self.device.handle();

        let to_transfer = image.transition(
            from_layout,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::AccessFlags::empty(),
            vk::AccessFlags::TRANSFER_WRITE,
        );
        let to_shader = image.transition(
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            vk::AccessFlags::TRANSFER_WRITE,
            vk::AccessFlags::SHADER_READ,
        );

        let copy = vk::BufferImageCopy::default()
            // Where the region starts within the staged buffer.
            .buffer_offset((y as usize * stride + x as usize * 4) as vk::DeviceSize)
            // In pixels, not bytes: this is how a partial update reads the
            // right rows out of a full-size source.
            .buffer_row_length(image.width())
            .buffer_image_height(image.height())
            .image_subresource(vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            })
            .image_offset(vk::Offset3D { x, y, z: 0 })
            .image_extent(vk::Extent3D {
                width: w,
                height: h,
                depth: 1,
            });

        unsafe {
            handle.cmd_pipeline_barrier(
                buffer,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_transfer],
            );
            handle.cmd_copy_buffer_to_image(
                buffer,
                staging.handle(),
                image.handle(),
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[copy],
            );
            handle.cmd_pipeline_barrier(
                buffer,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_shader],
            );
        }

        self.commands.submit()?;
        self.commands.wait(std::time::Duration::from_secs(5))?;
        Ok(())
    }
}

/// Pixels downloaded from the GPU, still in the buffer they landed in.
///
/// Held rather than copied out: the memory is host-visible and already
/// mapped, so `map_texture` hands back a slice of it directly.
pub struct VulkanMapping {
    buffer: crate::staging::Staging,
    width: u32,
    height: u32,
    fourcc: Fourcc,
    /// Bytes actually occupied, which is less than the buffer where the
    /// allocator rounded the size up.
    len: usize,
}

impl std::fmt::Debug for VulkanMapping {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VulkanMapping")
            .field("size", &(self.width, self.height))
            .field("format", &self.fourcc)
            .finish()
    }
}

impl Texture for VulkanMapping {
    fn width(&self) -> u32 {
        self.width
    }

    fn height(&self) -> u32 {
        self.height
    }

    fn format(&self) -> Option<Fourcc> {
        Some(self.fourcc)
    }
}

impl smithay::backend::renderer::TextureMapping for VulkanMapping {
    fn flipped(&self) -> bool {
        // The copy reads rows in image order, so what comes out is the same
        // way up as what went in.
        false
    }
}

impl ExportMem for VulkanRenderer {
    type TextureMapping = VulkanMapping;

    fn copy_framebuffer(
        &mut self,
        target: &Self::Framebuffer<'_>,
        region: Rectangle<i32, BufferCoord>,
        format: Fourcc,
    ) -> Result<Self::TextureMapping, Self::Error> {
        let image = target.image.clone();
        self.download(&image, region, format, vk::ImageLayout::GENERAL)
    }

    fn copy_texture(
        &mut self,
        texture: &Self::TextureId,
        region: Rectangle<i32, BufferCoord>,
        format: Fourcc,
    ) -> Result<Self::TextureMapping, Self::Error> {
        let image = texture.image.clone();
        self.download(
            &image,
            region,
            format,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        )
    }

    fn can_read_texture(&mut self, texture: &Self::TextureId) -> Result<bool, Self::Error> {
        // An imported dmabuf is created without TRANSFER_SRC, because asking
        // for it can make a modifier that would otherwise work be refused.
        Ok(texture.image.is_readable())
    }

    fn map_texture<'a>(
        &mut self,
        texture_mapping: &'a Self::TextureMapping,
    ) -> Result<&'a [u8], Self::Error> {
        Ok(&texture_mapping.buffer.read()[..texture_mapping.len])
    }
}

impl VulkanRenderer {
    /// Copy part of an image back into host memory.
    ///
    /// Waits for the copy: the caller is handed a slice, so there is nowhere
    /// to put a fence. Read-back is a screenshot or a screencopy request
    /// rather than something on the frame path, so the stall is in the right
    /// place.
    fn download(
        &mut self,
        image: &Image,
        region: Rectangle<i32, BufferCoord>,
        format: Fourcc,
        from_layout: vk::ImageLayout,
    ) -> Result<VulkanMapping, Error> {
        if !image.is_readable() {
            return Err(Error::Unsupported(
                "this image was imported without transfer support and cannot be read back"
                    .to_owned(),
            ));
        }
        // Converting formats during the copy would mean a shader pass. The
        // trait permits refusing, and a caller asking for the format the
        // texture already has is the case that matters.
        if format != image.fourcc() {
            return Err(Error::Unsupported(format!(
                "cannot convert {:?} to {format:?} while copying",
                image.fourcc()
            )));
        }

        let (x, y) = (region.loc.x.max(0), region.loc.y.max(0));
        let (w, h) = (region.size.w.max(0) as u32, region.size.h.max(0) as u32);
        if w == 0 || h == 0 {
            return Err(Error::Unsupported("an empty copy region".to_owned()));
        }
        if x as u32 + w > image.width() || y as u32 + h > image.height() {
            return Err(Error::Unsupported(format!(
                "region {region:?} is outside the {}x{} image",
                image.width(),
                image.height()
            )));
        }

        // Tightly packed: the mapping's stride is its width, which is what
        // `map_texture`'s caller assumes.
        let len = (w as usize) * (h as usize) * 4;
        let buffer = crate::staging::Staging::new(&self.device, len as vk::DeviceSize)?;

        let command = self.commands.begin()?;
        let handle = self.device.handle();

        let to_src = image.transition(
            from_layout,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            vk::AccessFlags::empty(),
            vk::AccessFlags::TRANSFER_READ,
        );
        // Put it back, because a copy must not be destructive: the framebuffer
        // may be scanned out and the texture may be drawn again.
        let restore = image.transition(
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            from_layout,
            vk::AccessFlags::TRANSFER_READ,
            vk::AccessFlags::empty(),
        );

        let copy = vk::BufferImageCopy::default()
            .buffer_offset(0)
            // Zero means tightly packed to the image extent.
            .buffer_row_length(0)
            .buffer_image_height(0)
            .image_subresource(vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            })
            .image_offset(vk::Offset3D { x, y, z: 0 })
            .image_extent(vk::Extent3D {
                width: w,
                height: h,
                depth: 1,
            });

        unsafe {
            handle.cmd_pipeline_barrier(
                command,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_src],
            );
            handle.cmd_copy_image_to_buffer(
                command,
                image.handle(),
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                buffer.handle(),
                &[copy],
            );
            handle.cmd_pipeline_barrier(
                command,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[restore],
            );
        }

        self.commands.submit()?;
        self.commands.wait(std::time::Duration::from_secs(5))?;

        Ok(VulkanMapping {
            buffer,
            width: w,
            height: h,
            fourcc: format,
            len,
        })
    }
}

impl Bind<Dmabuf> for VulkanRenderer {
    fn bind<'a>(&mut self, target: &'a mut Dmabuf) -> Result<Self::Framebuffer<'a>, Self::Error> {
        self.reap();

        let image = match self
            .targets
            .iter()
            .find(|(weak, _)| weak.upgrade().as_ref() == Some(target))
        {
            Some((_, image)) => image.clone(),
            None => {
                let image = std::sync::Arc::new(Image::import(&self.device, target, Purpose::Render)?);
                self.targets.push((target.weak(), image.clone()));
                image
            }
        };

        Ok(VulkanFramebuffer {
            image,
            _borrow: std::marker::PhantomData,
        })
    }

    fn supported_formats(&self) -> Option<smithay::backend::allocator::format::FormatSet> {
        // Renderable rather than sampleable: these are the formats an output
        // buffer may use.
        Some(
            format::COMMON_FORMATS
                .iter()
                .flat_map(|&code| {
                    format::modifiers(self.device.physical(), code)
                        .into_iter()
                        .filter(|s| s.rendering && s.planes == 1)
                        .map(move |s| Format {
                            code,
                            modifier: s.modifier,
                        })
                })
                .collect(),
        )
    }
}

impl smithay::backend::renderer::Offscreen<Dmabuf> for VulkanRenderer {
    fn create_buffer(
        &mut self,
        format: Fourcc,
        size: Size<i32, BufferCoord>,
    ) -> Result<Dmabuf, Self::Error> {
        // Renderable modifiers only: the whole point of an offscreen buffer is
        // that something will be drawn into it.
        let modifiers: Vec<_> = format::modifiers(self.device.physical(), format)
            .into_iter()
            .filter(|s| s.rendering && s.planes == 1)
            .map(|s| s.modifier)
            .collect();
        if modifiers.is_empty() {
            return Err(Error::Unsupported(format!(
                "{format:?} cannot be rendered into on {}",
                self.device.name()
            )));
        }

        let allocator = self.allocator.as_mut().ok_or_else(|| {
            Error::Unsupported(
                "this renderer was built without an allocator; use VulkanRenderer::with_allocator"
                    .to_owned(),
            )
        })?;
        Ok(allocator.allocate(
            size.w.max(0) as u32,
            size.h.max(0) as u32,
            format,
            &modifiers,
        )?)
    }
}

impl smithay::backend::renderer::Blit for VulkanRenderer {
    fn blit(
        &mut self,
        from: &Self::Framebuffer<'_>,
        to: &mut Self::Framebuffer<'_>,
        src: Rectangle<i32, Physical>,
        dst: Rectangle<i32, Physical>,
        filter: TextureFilter,
    ) -> Result<SyncPoint, Self::Error> {
        let source = from.image.clone();
        let target = to.image.clone();

        if std::sync::Arc::ptr_eq(&source, &target) {
            // Reading and writing one image in a single blit is undefined, and
            // the trait lists it as a failure.
            return Err(Error::Unsupported(
                "the source and destination framebuffers are the same image".to_owned(),
            ));
        }
        if !source.is_readable() {
            return Err(Error::Unsupported(
                "the source framebuffer's modifier does not support being copied from".to_owned(),
            ));
        }
        if !target.is_writable() {
            return Err(Error::Unsupported(
                "the destination framebuffer's modifier does not support being copied into"
                    .to_owned(),
            ));
        }

        let command = self.commands.begin()?;
        let handle = self.device.handle();

        // Both start in GENERAL, which is where release_barrier leaves an
        // image and where an externally written one is.
        let to_src = source.transition(
            vk::ImageLayout::GENERAL,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            vk::AccessFlags::empty(),
            vk::AccessFlags::TRANSFER_READ,
        );
        let to_dst = target.transition(
            vk::ImageLayout::GENERAL,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::AccessFlags::empty(),
            vk::AccessFlags::TRANSFER_WRITE,
        );
        // Put both back: a blit must leave the source intact and the
        // destination usable by whoever holds it.
        let restore_src = source.transition(
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            vk::ImageLayout::GENERAL,
            vk::AccessFlags::TRANSFER_READ,
            vk::AccessFlags::empty(),
        );
        let restore_dst = target.transition(
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::ImageLayout::GENERAL,
            vk::AccessFlags::TRANSFER_WRITE,
            vk::AccessFlags::empty(),
        );

        let layers = vk::ImageSubresourceLayers {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            mip_level: 0,
            base_array_layer: 0,
            layer_count: 1,
        };
        let region = vk::ImageBlit::default()
            .src_subresource(layers)
            .src_offsets([
                vk::Offset3D {
                    x: src.loc.x,
                    y: src.loc.y,
                    z: 0,
                },
                vk::Offset3D {
                    x: src.loc.x + src.size.w,
                    y: src.loc.y + src.size.h,
                    z: 1,
                },
            ])
            .dst_subresource(layers)
            .dst_offsets([
                vk::Offset3D {
                    x: dst.loc.x,
                    y: dst.loc.y,
                    z: 0,
                },
                vk::Offset3D {
                    x: dst.loc.x + dst.size.w,
                    y: dst.loc.y + dst.size.h,
                    z: 1,
                },
            ]);

        // vkCmdBlitImage scales, which is the reason to use it over a plain
        // copy: the sizes are allowed to differ.
        let filter = match filter {
            TextureFilter::Linear => vk::Filter::LINEAR,
            TextureFilter::Nearest => vk::Filter::NEAREST,
        };

        unsafe {
            handle.cmd_pipeline_barrier(
                command,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_src, to_dst],
            );
            handle.cmd_blit_image(
                command,
                source.handle(),
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                target.handle(),
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[region],
                filter,
            );
            handle.cmd_pipeline_barrier(
                command,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[restore_src, restore_dst],
            );
        }
        self.commands.submit()?;

        match self.commands.export_fence() {
            Ok(Some(fd)) => Ok(SyncPoint::from(crate::sync::SyncFile::new(fd))),
            Ok(None) => Ok(SyncPoint::signaled()),
            Err(_) => {
                self.commands.wait(std::time::Duration::from_secs(5))?;
                Ok(SyncPoint::signaled())
            }
        }
    }
}

/// A frame in progress.
pub struct VulkanFrame<'frame, 'buffer> {
    renderer: &'frame mut VulkanRenderer,
    framebuffer: &'frame mut VulkanFramebuffer<'buffer>,
    /// The framebuffer, in pixels: the size `Renderer::render` was given, which
    /// is what GLES sets its viewport to before it looks at the transform.
    output_size: Size<i32, Physical>,
    /// The same framebuffer as the desktop sees it — `output_size` with the
    /// transform applied, so portrait on a screen rotated 90 degrees. Every
    /// rectangle a caller hands in is in this space: it is what
    /// `Frame::output_size` reports and what Smithay's damage tracker lays
    /// elements out against.
    logical_size: Size<i32, Physical>,
    /// The output transform this frame was begun with. Every position map is
    /// built through it, so a rotated display needs nothing else.
    transform: Transform,
    finished: bool,
}

impl std::fmt::Debug for VulkanFrame<'_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VulkanFrame")
            .field("output_size", &self.output_size)
            .finish()
    }
}

impl<'frame, 'buffer> VulkanFrame<'frame, 'buffer> {
    fn begin(
        renderer: &'frame mut VulkanRenderer,
        framebuffer: &'frame mut VulkanFramebuffer<'buffer>,
        output_size: Size<i32, Physical>,
        transform: Transform,
    ) -> Result<Self, Error> {
        let target = framebuffer.image.clone();
        let buffer = renderer.commands.begin()?;
        let device = renderer.device.clone();
        let handle = device.handle();

        // The target may still be owned by whoever allocated it.
        let acquire = target.acquire_barrier(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL);
        unsafe {
            handle.cmd_pipeline_barrier(
                buffer,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[acquire],
            );
        }

        // LOAD rather than CLEAR: Smithay clears explicitly through
        // `Frame::clear`, and damage-tracked rendering depends on what was
        // already there surviving.
        let attachment = vk::RenderingAttachmentInfo::default()
            .image_view(target.view())
            .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .load_op(vk::AttachmentLoadOp::LOAD)
            .store_op(vk::AttachmentStoreOp::STORE);
        let area = vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: vk::Extent2D {
                width: target.width(),
                height: target.height(),
            },
        };
        let attachments = [attachment];
        let rendering = vk::RenderingInfo::default()
            .render_area(area)
            .layer_count(1)
            .color_attachments(&attachments);

        unsafe {
            device
                .dynamic_rendering()
                .cmd_begin_rendering(buffer, &rendering);
            handle.cmd_set_viewport(
                buffer,
                0,
                &[vk::Viewport {
                    x: 0.0,
                    y: 0.0,
                    width: target.width() as f32,
                    height: target.height() as f32,
                    min_depth: 0.0,
                    max_depth: 1.0,
                }],
            );
            handle.cmd_set_scissor(buffer, 0, &[area]);
        }

        Ok(Self {
            renderer,
            framebuffer,
            output_size,
            logical_size: transform.transform_size(output_size),
            transform,
            finished: false,
        })
    }

    fn command_buffer(&self) -> vk::CommandBuffer {
        self.renderer.commands.buffer()
    }

    /// Restrict drawing to `damage`, in target pixels.
    ///
    /// Smithay passes damage per draw call rather than per frame, so scissor
    /// is set immediately before each draw and covers the whole target again
    /// afterwards.
    /// Scissor a draw to its damage.
    ///
    /// The damage rectangles handed to `draw_solid` and
    /// `render_texture_from_to` are relative to `dst`, not to the
    /// framebuffer — Smithay's own GLES renderer translates by `dst.loc` and
    /// constrains to `dst.size` before using them (`gles/mod.rs:2756`).
    /// Treating them as framebuffer coordinates is right only while `dst.loc`
    /// is zero, which it is for every element that starts at the output's own
    /// origin. The shell does not: it is one buffer across the whole layout,
    /// so on the second monitor it is drawn at minus that monitor's position
    /// and every damage rectangle lands a screen's width outside the
    /// framebuffer. Nothing is drawn and the output shows the clear colour.
    fn scissors_within(
        &self,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
    ) -> Vec<Rectangle<i32, Physical>> {
        // No damage means nothing to draw, which is how Smithay's own renderer
        // reads it (`gles/mod.rs:2452`). Treating it as "the whole
        // destination" instead is not the safe direction it looks: the clear
        // goes through here too, and a clear with no damage then wipes the
        // output every frame. With a nearly fullscreen opaque window there is
        // frequently nothing to clear, so the desktop is erased and only what
        // was damaged that frame is drawn back.
        let clipped: Vec<Rectangle<i32, Physical>> = damage
            .iter()
            .filter_map(|rect| Rectangle::new(rect.loc + dst.loc, rect.size).intersection(dst))
            .collect();

        // Still desktop space at this point. The scissor is in framebuffer
        // coordinates, and the two differ whenever the transform swaps axes —
        // scissoring a rotated output with its own rectangle leaves most of
        // the framebuffer untouched.
        let rects: Vec<Rectangle<i32, Physical>> = clipped
            .into_iter()
            .map(|rect| {
                crate::transform::framebuffer_rect(rect, self.output_size, self.transform)
            })
            .collect();

        rects
    }

    /// Set the one scissor the pipeline has.
    ///
    /// `scissor_count` is 1, so `cmd_set_scissor` with an array sets registers
    /// the pipeline never reads: everything after the first rectangle is
    /// silently ignored, and the geometry it was meant to clip is simply not
    /// drawn. Damage arrives as several rectangles the moment a window sits in
    /// front of something — the region around it is a frame, not a box — so
    /// the caller draws once per rectangle instead.
    fn set_scissor(&self, rects: &[Rectangle<i32, Physical>]) {
        debug_assert!(rects.len() <= 1, "the pipeline has one scissor");
        let target = &self.framebuffer.image;
        let scissors: Vec<vk::Rect2D> = if rects.is_empty() {
            vec![vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent: vk::Extent2D {
                    width: target.width(),
                    height: target.height(),
                },
            }]
        } else {
            rects
                .iter()
                .map(|r| vk::Rect2D {
                    offset: vk::Offset2D {
                        x: r.loc.x.max(0),
                        y: r.loc.y.max(0),
                    },
                    extent: vk::Extent2D {
                        width: r.size.w.max(0) as u32,
                        height: r.size.h.max(0) as u32,
                    },
                })
                .collect()
        };
        unsafe {
            self.renderer
                .device
                .handle()
                .cmd_set_scissor(self.command_buffer(), 0, &scissors);
        }
    }
}

impl Frame for VulkanFrame<'_, '_> {
    type Error = Error;
    type TextureId = VulkanTexture;

    fn context_id(&self) -> ContextId<Self::TextureId> {
        self.renderer.context_id.clone()
    }

    fn clear(&mut self, color: Color32F, at: &[Rectangle<i32, Physical>]) -> Result<(), Self::Error> {
        // A solid draw rather than vkCmdClearAttachments: the blend state is
        // already right, and for an opaque colour — which is what a clear
        // always is in practice — the result is identical.
        //
        // The rectangle is the *output* size, not the framebuffer's.
        // draw_solid takes output-space coordinates and puts them through the
        // transform, and the two sizes differ whenever the transform swaps
        // axes — so using the framebuffer size here left a rotated output
        // partly uncleared.
        // Nothing to clear. Not the same as "clear everything" — see
        // `scissors_within`.
        if at.is_empty() {
            return Ok(());
        }
        let whole = Rectangle::from_size(self.logical_size);
        for rect in at {
            // The destination is the whole output and the rectangle is the
            // damage, so a clear of several regions is several draws of the
            // same quad rather than several quads.
            self.draw_solid(whole, std::slice::from_ref(rect), color)?;
        }
        Ok(())
    }

    fn draw_solid(
        &mut self,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        color: Color32F,
    ) -> Result<(), Self::Error> {
        let scissors = self.scissors_within(dst, damage);
        if scissors.is_empty() {
            return Ok(());
        }

        let target_format = self.framebuffer.image.format();
        let pipeline = self
            .renderer
            .pipelines
            .get(target_format, crate::pipeline::Kind::Solid)?;
        let position = crate::transform::position(dst, self.output_size, self.transform);
        let push = crate::pipeline::Push::new(
            position,
            // Unused by the solid pipeline, but the block is one shape.
            crate::transform::Affine {
                a: [1.0, 0.0, 0.0, 1.0],
                b: [0.0, 0.0, 0.0, 0.0],
            },
            // Smithay's Color32F is already premultiplied.
            [color.r(), color.g(), color.b(), color.a()],
            1.0,
        );

        let buffer = self.command_buffer();
        let layout = self.renderer.pipelines.layout();
        let handle = self.renderer.device.handle();
        unsafe {
            handle.cmd_bind_pipeline(buffer, vk::PipelineBindPoint::GRAPHICS, pipeline);
            handle.cmd_push_constants(
                buffer,
                layout,
                vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                0,
                push.as_bytes(),
            );
            // One draw per rectangle. The pipeline declares a single scissor,
            // so setting an array of them leaves everything after the first
            // silently unclipped-to — see `set_scissor`.
            for rect in &scissors {
                self.set_scissor(std::slice::from_ref(rect));
                handle.cmd_draw(buffer, 4, 1, 0, 0);
            }
        }

        self.set_scissor(&[]);
        Ok(())
    }

    fn render_texture_from_to(
        &mut self,
        texture: &Self::TextureId,
        src: Rectangle<f64, BufferCoord>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
        src_transform: Transform,
        alpha: f32,
    ) -> Result<(), Self::Error> {
        let scissors = self.scissors_within(dst, damage);
        if scissors.is_empty() {
            return Ok(());
        }

        let position = crate::transform::position(dst, self.output_size, self.transform);
        let texcoord = crate::transform::texture(
            src,
            (texture.width() as f64, texture.height() as f64),
            src_transform,
            texture.flipped,
        );
        // White tint: the texture's own colours, scaled by alpha.
        let push = crate::pipeline::Push::new(position, texcoord, [1.0, 1.0, 1.0, 1.0], alpha)
            .with_color(&texture.description, &self.renderer.output)
            .with_opaque(!texture.image.has_alpha());

        let target_format = self.framebuffer.image.format();
        let pipeline = self
            .renderer
            .pipelines
            .get(target_format, crate::pipeline::Kind::Texture)?;
        let layout = self.renderer.pipelines.layout();
        let sampler = self.renderer.pipelines.sampler();
        let buffer = self.command_buffer();
        let device = self.renderer.device.clone();
        let handle = device.handle();

        let image_info = vk::DescriptorImageInfo::default()
            .sampler(sampler)
            .image_view(texture.image.view())
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
        let infos = [image_info];
        let write = vk::WriteDescriptorSet::default()
            .dst_binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .image_info(&infos);

        unsafe {
            handle.cmd_bind_pipeline(buffer, vk::PipelineBindPoint::GRAPHICS, pipeline);
            device.push_descriptor().cmd_push_descriptor_set(
                buffer,
                vk::PipelineBindPoint::GRAPHICS,
                layout,
                0,
                &[write],
            );
            handle.cmd_push_constants(
                buffer,
                layout,
                vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                0,
                push.as_bytes(),
            );
            // One draw per rectangle, as in draw_solid.
            for rect in &scissors {
                self.set_scissor(std::slice::from_ref(rect));
                handle.cmd_draw(buffer, 4, 1, 0, 0);
            }
        }

        self.set_scissor(&[]);
        Ok(())
    }

    fn transformation(&self) -> Transform {
        self.transform
    }

    // Returning `logical_size` from something called `output_size` is the
    // point, not a slip: see below.
    #[allow(clippy::misnamed_getters)]
    fn output_size(&self) -> Size<i32, Physical> {
        // The transformed size, as GLES reports it (`gles/mod.rs` swaps the
        // axes before storing it). A caller that clears
        // `Rectangle::from_size(frame.output_size())` on a rotated screen is
        // asking for the whole desktop, not the whole framebuffer.
        self.logical_size
    }

    fn wait(&mut self, sync: &SyncPoint) -> Result<(), Self::Error> {
        // Queued onto the submission this frame will make at finish(), so a
        // client's acquire fence delays the GPU rather than this thread.
        self.renderer.wait_for(sync)
    }

    fn finish(mut self) -> Result<SyncPoint, Self::Error> {
        self.finished = true;

        let target = self.framebuffer.image.clone();
        let buffer = self.command_buffer();
        let device = self.renderer.device.clone();
        let handle = device.handle();

        unsafe {
            device.dynamic_rendering().cmd_end_rendering(buffer);
            let release = target.release_barrier(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL);
            handle.cmd_pipeline_barrier(
                buffer,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[release],
            );
        }
        self.renderer.commands.submit()?;

        // Hand back a fence rather than waiting for one. The caller passes the
        // fd to KMS, which waits on it in hardware — nothing on the CPU blocks
        // between submitting a frame and starting the next.
        //
        // A driver that cannot export leaves us with the CPU wait, which is
        // slower and still correct.
        match self.renderer.commands.export_fence() {
            Ok(Some(fd)) => Ok(SyncPoint::from(crate::sync::SyncFile::new(fd))),
            Ok(None) => Ok(SyncPoint::signaled()),
            Err(e) => {
                tracing::debug!("no exportable fence ({e:#}); falling back to a CPU wait");
                self.renderer
                    .commands
                    .wait(std::time::Duration::from_secs(5))?;
                Ok(SyncPoint::signaled())
            }
        }
    }
}

impl Drop for VulkanFrame<'_, '_> {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        // A dropped frame has an open render pass and an unsubmitted command
        // buffer. Ending and submitting is the least surprising of the
        // behaviours the trait permits: the alternative leaves the next
        // `begin` re-recording a buffer mid-pass.
        let device = self.renderer.device.clone();
        unsafe {
            device
                .dynamic_rendering()
                .cmd_end_rendering(self.renderer.commands.buffer());
        }
        let _ = self.renderer.commands.submit();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{gbm_allocator, require_gpu, skip, TestGpu};

    use smithay::backend::allocator::dmabuf::{AsDmabuf, DmabufMappingMode, DmabufSyncFlags};
    use smithay::backend::allocator::{Allocator, Modifier};
    use smithay::utils::Point;

    #[test]
    fn errors_carry_their_cause() {
        let error = Error::Unsupported("output transform 90 is not implemented".to_owned());
        assert!(error.to_string().contains("transform"));
    }

    struct Harness {
        renderer: VulkanRenderer,
        allocator: smithay::backend::allocator::gbm::GbmAllocator<std::fs::File>,
    }


    fn harness() -> Option<Harness> {
        let TestGpu { device, node } = require_gpu()?;
        let allocator = gbm_allocator(&node)?;

        let linear = |want_render: bool| {
            format::modifiers(device.physical(), Fourcc::Argb8888)
                .into_iter()
                .any(|s| {
                    s.modifier == Modifier::Linear
                        && if want_render { s.rendering } else { s.sampling }
                })
        };
        if !linear(true) || !linear(false) {
            skip("no linear ARGB8888 that is both renderable and sampleable");
            return None;
        }

        Some(Harness {
            renderer: VulkanRenderer::new(&device).expect("renderer"),
            allocator,
        })
    }

    fn buffer(
        allocator: &mut smithay::backend::allocator::gbm::GbmAllocator<std::fs::File>,
        w: u32,
        h: u32,
    ) -> Dmabuf {
        allocator
            .create_buffer(w, h, Fourcc::Argb8888, &[Modifier::Linear])
            .expect("gbm allocation")
            .export()
            .expect("export")
    }

    /// The whole of a rectangle, as damage.
    ///
    /// Damage is stated explicitly in every test rather than left empty. An
    /// empty damage list means "draw nothing" — that is what Smithay's own
    /// renderer does with it — and these tests previously relied on it meaning
    /// the opposite, which is how the compositor came to wipe its own output
    /// on every frame that had nothing to clear.
    fn all(w: i32, h: i32) -> [Rectangle<i32, Physical>; 1] {
        [Rectangle::from_size(Size::from((w, h)))]
    }

    /// One pixel as it sits in memory: B, G, R, A.
    fn pixel(buf: &Dmabuf, x: usize, y: usize) -> [u8; 4] {
        buf.sync_plane(0, DmabufSyncFlags::START | DmabufSyncFlags::READ)
            .expect("sync");
        let map = buf.map_plane(0, DmabufMappingMode::READ).expect("map");
        let stride = buf.strides().next().expect("stride") as usize;
        let bytes = unsafe { std::slice::from_raw_parts(map.ptr() as *const u8, map.length()) };
        let at = y * stride + x * 4;
        let out = [bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]];
        drop(map);
        let _ = buf.sync_plane(0, DmabufSyncFlags::END | DmabufSyncFlags::READ);
        out
    }

    fn fill(buf: &Dmabuf, color: [u8; 4], w: usize, h: usize) {
        buf.sync_plane(0, DmabufSyncFlags::START | DmabufSyncFlags::WRITE)
            .expect("sync");
        let map = buf.map_plane(0, DmabufMappingMode::WRITE).expect("map");
        let stride = buf.strides().next().expect("stride") as usize;
        let bytes = unsafe { std::slice::from_raw_parts_mut(map.ptr() as *mut u8, map.length()) };
        for y in 0..h {
            for x in 0..w {
                let at = y * stride + x * 4;
                bytes[at..at + 4].copy_from_slice(&color);
            }
        }
        drop(map);
        let _ = buf.sync_plane(0, DmabufSyncFlags::END | DmabufSyncFlags::WRITE);
    }

    /// The whole trait surface, driven the way Smithay drives it.
    #[test]
    fn a_frame_through_the_trait_api_clears_and_draws() {
        let Some(mut h) = harness() else { return };
        let mut target = buffer(&mut h.allocator, 64, 64);

        let mut framebuffer = h.renderer.bind(&mut target).expect("bind");
        let mut frame = h
            .renderer
            .render(&mut framebuffer, (64, 64).into(), Transform::Normal)
            .expect("render");

        frame
            .clear(Color32F::from([0.0, 0.0, 0.0, 1.0]), &all(frame.output_size().w, frame.output_size().h))
            .expect("clear");
        frame
            .draw_solid(
                Rectangle::new(Point::from((0, 0)), Size::from((32, 32))),
                &all(32, 32),
                Color32F::from([0.0, 1.0, 0.0, 1.0]),
            )
            .expect("draw_solid");
        let _ = frame.finish().expect("finish");
        drop(framebuffer);

        // Green is byte 1. Checked either side of the edge, so an off-by-one
        // or a vertical flip fails rather than looking plausible.
        assert_eq!(pixel(&target, 0, 0), [0, 255, 0, 255], "inside");
        assert_eq!(pixel(&target, 31, 31), [0, 255, 0, 255], "inside, corner");
        assert_eq!(pixel(&target, 32, 0), [0, 0, 0, 255], "right of it");
        assert_eq!(pixel(&target, 0, 32), [0, 0, 0, 255], "below it");
    }

    #[test]
    fn an_imported_dmabuf_can_be_rendered() {
        let Some(mut h) = harness() else { return };

        let source = buffer(&mut h.allocator, 32, 32);
        // B, G, R, A: pure blue.
        fill(&source, [255, 0, 0, 255], 32, 32);
        let texture = h.renderer.import_dmabuf(&source, None).expect("import");
        assert_eq!(texture.width(), 32);
        assert_eq!(texture.format(), Some(Fourcc::Argb8888));

        let mut target = buffer(&mut h.allocator, 64, 64);
        let mut framebuffer = h.renderer.bind(&mut target).expect("bind");
        let mut frame = h
            .renderer
            .render(&mut framebuffer, (64, 64).into(), Transform::Normal)
            .expect("render");

        frame
            .clear(Color32F::from([0.0, 0.0, 0.0, 1.0]), &all(frame.output_size().w, frame.output_size().h))
            .expect("clear");
        frame
            .render_texture_from_to(
                &texture,
                Rectangle::from_size(Size::from((32.0, 32.0))),
                Rectangle::new(Point::from((16, 16)), Size::from((32, 32))),
                &all(32, 32),
                &[],
                Transform::Normal,
                1.0,
            )
            .expect("render_texture_from_to");
        let _ = frame.finish().expect("finish");
        drop(framebuffer);

        assert_eq!(pixel(&target, 32, 32), [255, 0, 0, 255], "middle of texture");
        assert_eq!(pixel(&target, 8, 8), [0, 0, 0, 255], "outside it");
    }

    /// The second monitor's geometry: one buffer spanning every output, drawn
    /// at minus the output's position so each shows its own part.
    ///
    /// An output at x=2560 draws a 5120-wide shell at dst.x = -2560, and the
    /// only difference from the output at x=0 is that sign. If a negative
    /// destination is mishandled the first monitor is perfect and the second
    /// shows the clear colour, which reads as a dead output rather than as a
    /// rendering bug.
    #[test]
    fn a_texture_at_a_negative_offset_shows_its_far_side() {
        let Some(mut h) = harness() else { return };

        // Two halves, so which part landed on screen is visible in the pixel
        // and not just whether anything did.
        let source = buffer(&mut h.allocator, 64, 32);
        fill(&source, [255, 0, 0, 255], 64, 32);
        {
            source
                .sync_plane(0, DmabufSyncFlags::START | DmabufSyncFlags::WRITE)
                .expect("sync");
            let map = source.map_plane(0, DmabufMappingMode::WRITE).expect("map");
            let stride = source.strides().next().expect("stride") as usize;
            let bytes =
                unsafe { std::slice::from_raw_parts_mut(map.ptr() as *mut u8, map.length()) };
            for y in 0..32 {
                for x in 32..64 {
                    // Green: the right half.
                    bytes[y * stride + x * 4..y * stride + x * 4 + 4]
                        .copy_from_slice(&[0, 255, 0, 255]);
                }
            }
            drop(map);
            let _ = source.sync_plane(0, DmabufSyncFlags::END | DmabufSyncFlags::WRITE);
        }
        let texture = h.renderer.import_dmabuf(&source, None).expect("import");

        // A 32-wide "output" showing the second half of a 64-wide buffer.
        let mut target = buffer(&mut h.allocator, 32, 32);
        let mut framebuffer = h.renderer.bind(&mut target).expect("bind");
        let mut frame = h
            .renderer
            .render(&mut framebuffer, (32, 32).into(), Transform::Normal)
            .expect("render");
        frame
            .clear(Color32F::from([0.0, 0.0, 1.0, 1.0]), &all(frame.output_size().w, frame.output_size().h))
            .expect("clear");
        frame
            .render_texture_from_to(
                &texture,
                Rectangle::from_size(Size::from((64.0, 32.0))),
                Rectangle::new(Point::from((-32, 0)), Size::from((64, 32))),
                &all(64, 32),
                &[],
                Transform::Normal,
                1.0,
            )
            .expect("render_texture_from_to");
        let _ = frame.finish().expect("finish");
        drop(framebuffer);

        // Green everywhere: this output shows the buffer's right half only.
        for x in [1usize, 16, 30] {
            assert_eq!(
                pixel(&target, x, 16),
                [0, 255, 0, 255],
                "x={x} should be the texture's right half, not the clear colour"
            );
        }
    }

    /// The same geometry again, but through the damage tracker the compositor
    /// actually renders with rather than a bare frame.
    ///
    /// render_frame takes elements positioned relative to the output, so the
    /// shell — one buffer spanning every monitor — is handed to the second
    /// output at a negative x. The bare-frame test above proves the renderer
    /// draws that; this proves the tracker does not discard it on the way,
    /// which is the other way the second monitor ends up showing nothing but
    /// the clear colour.
    #[test]
    fn the_damage_tracker_keeps_an_element_that_starts_off_screen() {
        use smithay::backend::renderer::damage::OutputDamageTracker;
        use smithay::backend::renderer::element::texture::TextureRenderElement;
        use smithay::backend::renderer::element::{Id, Kind};
        use smithay::output::{Mode as OutputMode, Output, PhysicalProperties, Subpixel};

        let Some(mut h) = harness() else { return };

        let source = buffer(&mut h.allocator, 64, 32);
        fill(&source, [0, 255, 0, 255], 64, 32);
        let texture = h.renderer.import_dmabuf(&source, None).expect("import");

        let output = Output::new(
            "DP-3".to_owned(),
            PhysicalProperties {
                size: (600, 340).into(),
                subpixel: Subpixel::Unknown,
                make: "test".to_owned(),
                model: "test".to_owned(),
                serial_number: "test".to_owned(),
            },
        );
        let mode = OutputMode {
            size: (32, 32).into(),
            refresh: 60_000,
        };
        output.change_current_state(Some(mode), None, None, Some((32, 0).into()));
        output.set_preferred(mode);
        let mut tracker = OutputDamageTracker::from_output(&output);

        let element = TextureRenderElement::from_static_texture(
            Id::new(),
            h.renderer.context_id(),
            // Minus the output's position in the layout, as udev.rs does.
            (-32.0, 0.0),
            texture.clone(),
            1,
            Transform::Normal,
            None,
            None,
            None,
            None,
            Kind::Unspecified,
        );

        let mut target = buffer(&mut h.allocator, 32, 32);
        let mut framebuffer = h.renderer.bind(&mut target).expect("bind");
        tracker
            .render_output(
                &mut h.renderer,
                &mut framebuffer,
                0,
                &[element],
                Color32F::from([0.0, 0.0, 1.0, 1.0]),
            )
            .expect("render_output");
        drop(framebuffer);

        for x in [1usize, 16, 30] {
            assert_eq!(
                pixel(&target, x, 16),
                [0, 255, 0, 255],
                "x={x} should be the shell, not the clear colour"
            );
        }
    }

    /// A shell element with a stable id has to carry damage, or the second
    /// frame is never drawn.
    ///
    /// The id has to be stable: a fresh one per frame makes the tracker treat
    /// the shell as a new element every time and repaint the whole output for
    /// ever. But a stable id means the tracker decides whether to redraw by
    /// asking the element what changed, and one built with
    /// DamageSnapshot::empty() answers "nothing" for ever — the outputs go
    /// quiet after the first frame while WebKit carries on painting.
    #[test]
    fn a_stable_element_id_still_redraws_when_its_damage_changes() {
        use smithay::backend::renderer::damage::OutputDamageTracker;
        use smithay::backend::renderer::element::texture::TextureRenderElement;
        use smithay::backend::renderer::element::{Id, Kind};
        use smithay::backend::renderer::utils::DamageBag;
        use smithay::output::{Mode as OutputMode, Output, PhysicalProperties, Subpixel};

        let Some(mut h) = harness() else { return };

        let source = buffer(&mut h.allocator, 32, 32);
        fill(&source, [0, 255, 0, 255], 32, 32);
        let texture = h.renderer.import_dmabuf(&source, None).expect("import");

        let output = Output::new(
            "DP-1".to_owned(),
            PhysicalProperties {
                size: (600, 340).into(),
                subpixel: Subpixel::Unknown,
                make: "test".to_owned(),
                model: "test".to_owned(),
                serial_number: "test".to_owned(),
            },
        );
        let mode = OutputMode {
            size: (32, 32).into(),
            refresh: 60_000,
        };
        output.change_current_state(Some(mode), None, None, Some((0, 0).into()));
        output.set_preferred(mode);

        let id = Id::new();
        let mut bag: DamageBag<i32, BufferCoord> = DamageBag::default();
        let mut target = buffer(&mut h.allocator, 32, 32);

        let element = |bag: &DamageBag<i32, BufferCoord>, renderer: &VulkanRenderer| {
            TextureRenderElement::from_texture_with_damage(
                id.clone(),
                renderer.context_id(),
                (0.0, 0.0),
                texture.clone(),
                1,
                Transform::Normal,
                None,
                None,
                None,
                None,
                bag.snapshot(),
                Kind::Unspecified,
            )
        };

        let mut tracker = OutputDamageTracker::from_output(&output);
        // The buffer age matters: 0 means "the contents of this buffer are
        // unknown", which always reports full damage and would make every
        // assertion below pass regardless.
        let render = |h: &mut Harness,
                          tracker: &mut OutputDamageTracker,
                          target: &mut Dmabuf,
                          bag: &DamageBag<i32, BufferCoord>,
                          age: usize| {
            let el = element(bag, &h.renderer);
            let mut framebuffer = h.renderer.bind(target).expect("bind");
            let result = tracker
                .render_output(
                    &mut h.renderer,
                    &mut framebuffer,
                    age,
                    &[el],
                    Color32F::from([0.0, 0.0, 1.0, 1.0]),
                )
                .expect("render_output");
            let damaged = result.damage.map(|d| !d.is_empty()).unwrap_or(false);
            drop(framebuffer);
            damaged
        };

        bag.add([Rectangle::from_size(Size::from((32, 32)))]);
        assert!(
            render(&mut h, &mut tracker, &mut target, &bag, 0),
            "the first frame always draws"
        );

        // Nothing new: the shell has not painted, so there is nothing to do.
        assert!(
            !render(&mut h, &mut tracker, &mut target, &bag, 1),
            "an unchanged shell should not repaint"
        );

        // A new frame from WebKit. Without the damage this returns false and
        // the output stops for good.
        bag.add([Rectangle::from_size(Size::from((32, 32)))]);
        assert!(
            render(&mut h, &mut tracker, &mut target, &bag, 1),
            "a new shell frame has to redraw, or the display freezes"
        );
    }

    /// A small element in front must not stop a large one behind it from
    /// being drawn everywhere else.
    ///
    /// This is the shape of the compositor's own list: the shell is one buffer
    /// spanning every output and every window sits in front of part of it. If
    /// the element in front suppresses the one behind beyond its own
    /// rectangle, the desktop is replaced by the clear colour the moment a
    /// window opens.
    #[test]
    fn an_element_in_front_only_covers_its_own_rectangle() {
        use smithay::backend::renderer::damage::OutputDamageTracker;
        use smithay::backend::renderer::element::texture::TextureRenderElement;
        use smithay::backend::renderer::element::{Id, Kind};
        use smithay::backend::renderer::utils::DamageBag;
        use smithay::output::{Mode as OutputMode, Output, PhysicalProperties, Subpixel};

        let Some(mut h) = harness() else { return };

        // The "shell": wider than the output, as the real one is.
        let shell_buf = buffer(&mut h.allocator, 128, 64);
        fill(&shell_buf, [0, 255, 0, 255], 128, 64);
        let shell = h.renderer.import_dmabuf(&shell_buf, None).expect("shell");

        // The "window": small, opaque, in front.
        let win_buf = buffer(&mut h.allocator, 16, 16);
        fill(&win_buf, [0, 0, 255, 255], 16, 16);
        let window = h.renderer.import_dmabuf(&win_buf, None).expect("window");

        let output = Output::new(
            "DP-1".to_owned(),
            PhysicalProperties {
                size: (600, 340).into(),
                subpixel: Subpixel::Unknown,
                make: "test".to_owned(),
                model: "test".to_owned(),
                serial_number: "test".to_owned(),
            },
        );
        let mode = OutputMode {
            size: (64, 64).into(),
            refresh: 60_000,
        };
        output.change_current_state(Some(mode), None, None, Some((0, 0).into()));
        output.set_preferred(mode);
        let mut tracker = OutputDamageTracker::from_output(&output);

        let mut bag: DamageBag<i32, BufferCoord> = DamageBag::default();
        bag.add([Rectangle::from_size(Size::from((128, 64)))]);

        let shell_element = TextureRenderElement::from_texture_with_damage(
            Id::new(),
            h.renderer.context_id(),
            (0.0, 0.0),
            shell.clone(),
            1,
            Transform::Normal,
            None,
            None,
            None,
            None,
            bag.snapshot(),
            Kind::Unspecified,
        );
        let window_element = TextureRenderElement::from_static_texture(
            Id::new(),
            h.renderer.context_id(),
            (0.0, 0.0),
            window.clone(),
            1,
            Transform::Normal,
            None,
            None,
            None,
            None,
            Kind::Unspecified,
        );

        let mut target = buffer(&mut h.allocator, 64, 64);
        let mut framebuffer = h.renderer.bind(&mut target).expect("bind");
        tracker
            .render_output(
                &mut h.renderer,
                &mut framebuffer,
                0,
                // Front to back, as the compositor builds it.
                &[window_element, shell_element],
                Color32F::from([1.0, 0.0, 0.0, 1.0]),
            )
            .expect("render_output");
        drop(framebuffer);

        // Under the window: the window.
        assert_eq!(pixel(&target, 8, 8), [0, 0, 255, 255], "the window itself");
        // Everywhere else: the shell, not the clear colour.
        for (x, y) in [(40usize, 8usize), (8, 40), (40, 40), (60, 60)] {
            assert_eq!(
                pixel(&target, x, y),
                [0, 255, 0, 255],
                "({x},{y}) should be the shell, not the clear colour"
            );
        }
    }

    /// An X-format buffer is opaque, whatever is in the byte where alpha
    /// would be.
    ///
    /// Vulkan has no X formats, so XRGB8888 is imported as B8G8R8A8_UNORM and
    /// the fourth byte is sampled as alpha. Clients leave it zero — it is
    /// defined as ignored — so taking it at face value makes an opaque window
    /// vanish, leaving only whatever bytes happened to be non-zero. That is a
    /// terminal showing its glyphs over nothing at all.
    #[test]
    fn an_x_format_buffer_is_opaque_whatever_its_fourth_byte_says() {
        let Some(mut h) = harness() else { return };

        let source = h
            .allocator
            .create_buffer(32, 32, Fourcc::Xrgb8888, &[Modifier::Linear])
            .expect("gbm allocation")
            .export()
            .expect("export");
        // Blue, with the ignored byte left at zero as a client leaves it.
        fill(&source, [255, 0, 0, 0], 32, 32);

        let texture = h.renderer.import_dmabuf(&source, None).expect("import");
        assert_eq!(texture.format(), Some(Fourcc::Xrgb8888));

        let mut target = buffer(&mut h.allocator, 32, 32);
        let mut framebuffer = h.renderer.bind(&mut target).expect("bind");
        let mut frame = h
            .renderer
            .render(&mut framebuffer, (32, 32).into(), Transform::Normal)
            .expect("render");
        // Red underneath, so a transparent draw is obvious rather than black.
        frame
            .clear(Color32F::from([1.0, 0.0, 0.0, 1.0]), &all(frame.output_size().w, frame.output_size().h))
            .expect("clear");
        frame
            .render_texture_from_to(
                &texture,
                Rectangle::from_size(Size::from((32.0, 32.0))),
                Rectangle::new(Point::from((0, 0)), Size::from((32, 32))),
                &all(32, 32),
                &[],
                Transform::Normal,
                1.0,
            )
            .expect("render_texture_from_to");
        let _ = frame.finish().expect("finish");
        drop(framebuffer);

        assert_eq!(
            pixel(&target, 16, 16),
            [255, 0, 0, 255],
            "the buffer is opaque blue, not the red behind it"
        );
    }

    /// Damage arrives as several rectangles, and every one of them has to be
    /// drawn.
    ///
    /// The pipeline declares a single scissor, so handing cmd_set_scissor an
    /// array sets registers nothing reads: everything after the first
    /// rectangle is silently dropped. That is not a corner case — the moment a
    /// window sits in front of the shell, the region of the shell still
    /// visible is a frame around it, which is four rectangles. Only the first
    /// was drawn, so the desktop survived above the window and the strips
    /// beside and below it were left at the clear colour.
    #[test]
    fn every_damage_rectangle_is_drawn_not_just_the_first() {
        let Some(mut h) = harness() else { return };

        let source = buffer(&mut h.allocator, 64, 64);
        fill(&source, [0, 255, 0, 255], 64, 64);
        let texture = h.renderer.import_dmabuf(&source, None).expect("import");

        let mut target = buffer(&mut h.allocator, 64, 64);
        let mut framebuffer = h.renderer.bind(&mut target).expect("bind");
        let mut frame = h
            .renderer
            .render(&mut framebuffer, (64, 64).into(), Transform::Normal)
            .expect("render");
        frame
            .clear(Color32F::from([0.0, 0.0, 1.0, 1.0]), &all(frame.output_size().w, frame.output_size().h))
            .expect("clear");

        // A frame around a hole, as the visible part of the shell is when a
        // window covers its middle.
        let damage = [
            Rectangle::new(Point::from((0, 0)), Size::from((64, 8))),
            Rectangle::new(Point::from((0, 56)), Size::from((64, 8))),
            Rectangle::new(Point::from((0, 8)), Size::from((8, 48))),
            Rectangle::new(Point::from((56, 8)), Size::from((8, 48))),
        ];
        frame
            .render_texture_from_to(
                &texture,
                Rectangle::from_size(Size::from((64.0, 64.0))),
                Rectangle::new(Point::from((0, 0)), Size::from((64, 64))),
                &damage,
                &[],
                Transform::Normal,
                1.0,
            )
            .expect("render_texture_from_to");
        let _ = frame.finish().expect("finish");
        drop(framebuffer);

        // Every side of the frame.
        for (x, y, side) in [
            (32usize, 4usize, "top"),
            (32, 60, "bottom"),
            (4, 32, "left"),
            (60, 32, "right"),
        ] {
            assert_eq!(
                pixel(&target, x, y),
                [0, 255, 0, 255],
                "the {side} damage rectangle was not drawn"
            );
        }
        // The hole is not damaged, so it keeps the clear colour.
        assert_eq!(pixel(&target, 32, 32), [255, 0, 0, 255], "undamaged middle");
    }

    /// No damage means nothing to draw, and for the clear that is the whole
    /// difference between a desktop and a blank screen.
    ///
    /// Smithay clears only what it has worked out needs clearing, and with a
    /// nearly fullscreen opaque window in front of everything that is
    /// frequently nothing at all. Reading an empty list as "the whole output"
    /// erases the frame that was already there, and only what happened to be
    /// damaged that frame is drawn back — a terminal's text, and nothing else.
    #[test]
    fn a_clear_with_no_damage_leaves_the_frame_alone() {
        let Some(mut h) = harness() else { return };
        let mut target = buffer(&mut h.allocator, 32, 32);

        // A frame worth keeping.
        {
            let mut framebuffer = h.renderer.bind(&mut target).expect("bind");
            let mut frame = h
                .renderer
                .render(&mut framebuffer, (32, 32).into(), Transform::Normal)
                .expect("render");
            frame
                .clear(Color32F::from([0.0, 1.0, 0.0, 1.0]), &all(32, 32))
                .expect("clear");
            let _ = frame.finish().expect("finish");
        }
        assert_eq!(pixel(&target, 16, 16), [0, 255, 0, 255], "the frame before");

        // A frame with nothing to clear must not touch it.
        {
            let mut framebuffer = h.renderer.bind(&mut target).expect("bind");
            let mut frame = h
                .renderer
                .render(&mut framebuffer, (32, 32).into(), Transform::Normal)
                .expect("render");
            frame
                .clear(Color32F::from([1.0, 0.0, 0.0, 1.0]), &[])
                .expect("clear");
            let _ = frame.finish().expect("finish");
        }
        assert_eq!(
            pixel(&target, 16, 16),
            [0, 255, 0, 255],
            "an empty clear wiped the frame"
        );
    }

    #[test]
    fn importing_the_same_buffer_twice_reuses_the_image() {
        // Without this the renderer would allocate a fresh VkImage and a fresh
        // memory import for every surface, every frame.
        let Some(mut h) = harness() else { return };
        let source = buffer(&mut h.allocator, 16, 16);

        let first = h.renderer.import_dmabuf(&source, None).expect("first");
        let second = h.renderer.import_dmabuf(&source, None).expect("second");

        assert!(
            std::sync::Arc::ptr_eq(&first.image, &second.image),
            "the second import allocated a second image"
        );
    }

    /// A rotated display, drawn the way Smithay asks for it.
    ///
    /// `render` is given the *framebuffer* size and the transform, and the
    /// rectangles that follow are in the transformed space — the same way round
    /// as GLES, which sets its viewport to the size it was handed and only then
    /// swaps the axes for its projection.
    ///
    /// So a 32x16 framebuffer rotated 90 degrees holds a 16x32 desktop, and
    /// `transform_point_in` maps `(x, y)` in that desktop to `(32 - y, x)`.
    /// The top quarter of the desktop — y 0..8, full width — becomes the right
    /// strip of the framebuffer: x 24..32, y 0..16. An unrotated draw of the
    /// same rectangle would instead fill x 0..16, y 0..8, which is what the two
    /// "distinguishes" assertions below pin down.
    #[test]
    fn a_rotated_output_puts_pixels_where_the_rotation_says() {
        let Some(mut h) = harness() else { return };
        let mut target = buffer(&mut h.allocator, 32, 16);

        let mut framebuffer = h.renderer.bind(&mut target).expect("bind");
        let mut frame = h
            .renderer
            .render(&mut framebuffer, (32, 16).into(), Transform::_90)
            .expect("a rotated output must be supported");
        assert_eq!(frame.transformation(), Transform::_90);
        assert_eq!(
            frame.output_size(),
            Size::from((16, 32)),
            "a rotated frame reports the desktop it holds"
        );

        frame
            .clear(Color32F::from([0.0, 0.0, 0.0, 1.0]), &all(frame.output_size().w, frame.output_size().h))
            .expect("clear");
        frame
            .draw_solid(
                Rectangle::new(Point::from((0, 0)), Size::from((16, 8))),
                &all(16, 8),
                Color32F::from([0.0, 1.0, 0.0, 1.0]),
            )
            .expect("draw");
        let _ = frame.finish().expect("finish");
        drop(framebuffer);

        // Green is byte 1.
        assert_eq!(pixel(&target, 29, 2), [0, 255, 0, 255], "inside the rotated strip");
        assert_eq!(pixel(&target, 4, 2), [0, 0, 0, 255], "left of it");

        // The two pixels where a rotated and an unrotated draw disagree.
        assert_eq!(
            pixel(&target, 28, 12),
            [0, 255, 0, 255],
            "an unrotated draw would have missed this"
        );
        assert_eq!(
            pixel(&target, 4, 4),
            [0, 0, 0, 255],
            "an unrotated draw would have covered this"
        );
    }

    /// The clear covers the whole framebuffer whatever the rotation.
    #[test]
    fn every_output_transform_clears_the_whole_target() {
        let Some(mut h) = harness() else { return };
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
            // Square, so one buffer works for every transform.
            let mut target = buffer(&mut h.allocator, 32, 32);
            let mut framebuffer = h.renderer.bind(&mut target).expect("bind");
            let mut frame = h
                .renderer
                .render(&mut framebuffer, (32, 32).into(), transform)
                .unwrap_or_else(|e| panic!("{transform:?}: {e}"));
            frame
                .clear(Color32F::from([1.0, 0.0, 0.0, 1.0]), &all(frame.output_size().w, frame.output_size().h))
                .expect("clear");
            let _ = frame.finish().expect("finish");
            drop(framebuffer);

            for (x, y) in [(0, 0), (31, 0), (0, 31), (31, 31), (16, 16)] {
                assert_eq!(
                    pixel(&target, x, y),
                    [0, 0, 255, 255],
                    "{transform:?} left {x},{y} unclear"
                );
            }
        }
    }

    /// What a screenshot does: render, then read the framebuffer back.
    #[test]
    fn a_framebuffer_can_be_copied_back_into_memory() {
        use smithay::backend::renderer::TextureMapping as _;

        let Some(mut h) = harness() else { return };
        let mut target = buffer(&mut h.allocator, 32, 32);

        let mut framebuffer = h.renderer.bind(&mut target).expect("bind");
        let mut frame = h
            .renderer
            .render(&mut framebuffer, (32, 32).into(), Transform::Normal)
            .expect("render");
        frame
            .clear(Color32F::from([0.0, 0.0, 0.0, 1.0]), &all(frame.output_size().w, frame.output_size().h))
            .expect("clear");
        frame
            .draw_solid(
                Rectangle::new(Point::from((0, 0)), Size::from((16, 32))),
                &all(16, 32),
                Color32F::from([1.0, 0.0, 0.0, 1.0]),
            )
            .expect("draw");
        let _ = frame.finish().expect("finish");

        let mapping = h
            .renderer
            .copy_framebuffer(
                &framebuffer,
                Rectangle::new(Point::from((0, 0)), Size::from((32, 32))),
                Fourcc::Argb8888,
            )
            .expect("copy_framebuffer");
        assert_eq!(mapping.width(), 32);
        assert_eq!(mapping.height(), 32);
        assert!(!mapping.flipped());

        let pixels = h.renderer.map_texture(&mapping).expect("map_texture");
        // Tightly packed, so the stride is the width.
        assert_eq!(pixels.len(), 32 * 32 * 4);
        let at = |x: usize, y: usize| {
            let i = (y * 32 + x) * 4;
            [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
        };
        // Red is byte 2 in ARGB8888.
        assert_eq!(at(4, 4), [0, 0, 255, 255], "the drawn half");
        assert_eq!(at(24, 4), [0, 0, 0, 255], "the cleared half");
        drop(framebuffer);

        // Not destructive: the framebuffer still holds what was drawn.
        assert_eq!(pixel(&target, 4, 4), [0, 0, 255, 255]);
    }

    #[test]
    fn a_sub_region_copies_only_that_region() {
        let Some(mut h) = harness() else { return };
        let mut target = buffer(&mut h.allocator, 32, 32);
        let mut framebuffer = h.renderer.bind(&mut target).expect("bind");
        let mut frame = h
            .renderer
            .render(&mut framebuffer, (32, 32).into(), Transform::Normal)
            .expect("render");
        frame
            .clear(Color32F::from([0.0, 0.0, 1.0, 1.0]), &all(frame.output_size().w, frame.output_size().h))
            .expect("clear");
        let _ = frame.finish().expect("finish");

        let mapping = h
            .renderer
            .copy_framebuffer(
                &framebuffer,
                Rectangle::new(Point::from((8, 8)), Size::from((4, 4))),
                Fourcc::Argb8888,
            )
            .expect("copy");
        let pixels = h.renderer.map_texture(&mapping).expect("map");
        assert_eq!(pixels.len(), 4 * 4 * 4, "only the region is copied");
        // Blue is byte 0.
        assert_eq!(&pixels[0..4], &[255, 0, 0, 255]);
    }

    #[test]
    fn an_shm_texture_can_be_read_back() {
        let Some(mut h) = harness() else { return };

        // Allocated by this renderer, so it always has transfer support.
        let pixels: Vec<u8> = std::iter::repeat_n([0u8, 255, 0, 255], 8 * 8)
            .flatten()
            .collect();
        let shm = h
            .renderer
            .import_memory(&pixels, Fourcc::Argb8888, (8, 8).into(), false)
            .expect("import_memory");
        assert!(h.renderer.can_read_texture(&shm).expect("can_read"));

        let mapping = h
            .renderer
            .copy_texture(
                &shm,
                Rectangle::new(Point::from((0, 0)), Size::from((8, 8))),
                Fourcc::Argb8888,
            )
            .expect("copy_texture");
        let out = h.renderer.map_texture(&mapping).expect("map");
        assert_eq!(&out[0..4], &[0, 255, 0, 255], "green survived the round trip");
    }

    /// Readability of an imported buffer follows its modifier, not a guess.
    ///
    /// The transfer usages are only requested where the modifier advertises
    /// them, because asking for one it does not support makes vkCreateImage
    /// refuse a buffer that would otherwise have imported fine. So whether a
    /// dmabuf can be read back is a property of the modifier, and the two have
    /// to agree.
    #[test]
    fn dmabuf_readability_matches_what_the_modifier_advertises() {
        let Some(mut h) = harness() else { return };
        let source = buffer(&mut h.allocator, 8, 8);
        let texture = h.renderer.import_dmabuf(&source, None).expect("import");

        let advertised = format::modifiers(h.renderer.device().physical(), Fourcc::Argb8888)
            .into_iter()
            .find(|s| s.modifier == Modifier::Linear)
            .expect("linear ARGB8888 is supported, the harness checked")
            .transfer_src;

        let reported = h.renderer.can_read_texture(&texture).expect("can_read");
        assert_eq!(
            reported, advertised,
            "can_read_texture disagrees with the modifier's TRANSFER_SRC feature"
        );

        let result = h.renderer.copy_texture(
            &texture,
            Rectangle::new(Point::from((0, 0)), Size::from((8, 8))),
            Fourcc::Argb8888,
        );
        assert_eq!(
            result.is_ok(),
            advertised,
            "copy_texture and can_read_texture must agree"
        );
    }

    /// A fence handed to the renderer becomes a wait on the queue, not a
    /// stall on this thread.
    #[test]
    fn waiting_on_a_fence_defers_to_the_gpu() {
        let Some(mut h) = harness() else { return };

        // A real, not-yet-signalled fence: render into a buffer and take the
        // sync point without waiting for it.
        let mut source = buffer(&mut h.allocator, 64, 64);
        let sync = {
            let mut framebuffer = h.renderer.bind(&mut source).expect("bind");
            let mut frame = h
                .renderer
                .render(&mut framebuffer, (64, 64).into(), Transform::Normal)
                .expect("render");
            frame
                .clear(Color32F::from([1.0, 0.0, 0.0, 1.0]), &all(frame.output_size().w, frame.output_size().h))
                .expect("clear");
            frame.finish().expect("finish")
        };
        assert!(sync.contains_fence(), "the frame did not produce a fence");

        // Handing it over must queue a semaphore rather than block.
        assert_eq!(h.renderer.commands.pending_waits(), 0);
        h.renderer.wait(&sync).expect("wait");
        assert_eq!(
            h.renderer.commands.pending_waits(),
            1,
            "the fence was waited on by the CPU instead of the queue"
        );

        // The next submission consumes it and still produces correct pixels.
        let mut target = buffer(&mut h.allocator, 64, 64);
        let mut framebuffer = h.renderer.bind(&mut target).expect("bind");
        let mut frame = h
            .renderer
            .render(&mut framebuffer, (64, 64).into(), Transform::Normal)
            .expect("render");
        frame
            .clear(Color32F::from([0.0, 1.0, 0.0, 1.0]), &all(frame.output_size().w, frame.output_size().h))
            .expect("clear");
        let after = frame.finish().expect("finish");
        after.wait().expect("wait");
        drop(framebuffer);

        assert_eq!(
            h.renderer.commands.pending_waits(),
            0,
            "the wait was not consumed by the submission"
        );
        assert_eq!(pixel(&target, 32, 32), [0, 255, 0, 255]);
    }

    #[test]
    fn an_already_signalled_sync_point_queues_nothing() {
        let Some(mut h) = harness() else { return };
        h.renderer
            .wait(&SyncPoint::signaled())
            .expect("an already-reached sync point is not an error");
        assert_eq!(h.renderer.commands.pending_waits(), 0);
    }

    /// Draw a texture with a known description and check the result against
    /// what `color.rs` says it should be.
    ///
    /// This is the test that matters for colour: the shader is a translation
    /// of those functions, and nothing else checks that the translation is
    /// faithful. A drifted curve looks like a slightly washed-out image, which
    /// no other assertion here would catch.
    fn convert_and_read(
        h: &mut Harness,
        source: crate::color::Description,
        output: crate::color::Description,
        value: u8,
    ) -> [u8; 4] {
        // A texture holding one known value, opaque.
        let pixels: Vec<u8> = std::iter::repeat_n([value, value, value, 255], 8 * 8)
            .flatten()
            .collect();
        let texture = h
            .renderer
            .import_memory(&pixels, Fourcc::Argb8888, (8, 8).into(), false)
            .expect("import_memory")
            .with_description(source);

        h.renderer.set_output_description(output);

        let mut target = buffer(&mut h.allocator, 16, 16);
        let mut framebuffer = h.renderer.bind(&mut target).expect("bind");
        let mut frame = h
            .renderer
            .render(&mut framebuffer, (16, 16).into(), Transform::Normal)
            .expect("render");
        frame
            .clear(Color32F::from([0.0, 0.0, 0.0, 1.0]), &all(frame.output_size().w, frame.output_size().h))
            .expect("clear");
        frame
            .render_texture_from_to(
                &texture,
                Rectangle::from_size(Size::from((8.0, 8.0))),
                Rectangle::new(Point::from((0, 0)), Size::from((16, 16))),
                &all(16, 16),
                &[],
                Transform::Normal,
                1.0,
            )
            .expect("draw");
        let _ = frame.finish().expect("finish");
        drop(framebuffer);

        pixel(&target, 8, 8)
    }

    #[test]
    fn the_shader_agrees_with_the_cpu_colour_conversion() {
        use crate::color::{Description, Primaries, TransferFunction};

        let Some(mut h) = harness() else { return };

        let cases = [
            // Linear content onto an sRGB output: the encode alone.
            (
                Description {
                    transfer: TransferFunction::Linear,
                    ..Default::default()
                },
                Description::default(),
            ),
            // sRGB to sRGB: nothing should change.
            (Description::default(), Description::default()),
            // A wide gamut onto a narrow one: the primaries matrix as well.
            (
                Description {
                    transfer: TransferFunction::Srgb,
                    primaries: Primaries::BT2020,
                    ..Default::default()
                },
                Description::default(),
            ),
            // Gamma 2.2 content, which is close to sRGB but not equal to it.
            (
                Description {
                    transfer: TransferFunction::Gamma22,
                    ..Default::default()
                },
                Description::default(),
            ),
        ];

        for (source, output) in cases {
            for value in [64u8, 128, 200] {
                let got = convert_and_read(&mut h, source, output, value);

                let normalised = value as f32 / 255.0;
                let expected = source.convert(&output, [normalised; 3]);
                // ARGB8888 is B, G, R, A in memory, and the input is grey so
                // every channel should agree.
                let want = (expected[0].clamp(0.0, 1.0) * 255.0).round() as i32;

                for (channel, name) in [(got[0], "blue"), (got[1], "green"), (got[2], "red")] {
                    assert!(
                        (channel as i32 - want).abs() <= 3,
                        "{source:?} -> {output:?} at {value}: {name} was {channel}, \\
                         color.rs says {want}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_texture_in_the_output_space_is_passed_through_unchanged() {
        use crate::color::Description;

        let Some(mut h) = harness() else { return };
        // Identical descriptions: whatever the curve, the value must survive.
        let got = convert_and_read(&mut h, Description::default(), Description::default(), 128);
        for channel in &got[..3] {
            assert!(
                (*channel as i32 - 128).abs() <= 2,
                "an identity conversion changed 128 to {got:?}"
            );
        }
    }

    #[test]
    fn a_texture_defaults_to_sdr_srgb() {
        // What a client that says nothing is assumed to have sent.
        let Some(mut h) = harness() else { return };
        let source = buffer(&mut h.allocator, 8, 8);
        let texture = h.renderer.import_dmabuf(&source, None).expect("import");
        assert_eq!(
            *texture.description(),
            crate::color::Description::default()
        );
        assert_eq!(
            *h.renderer.output_description(),
            crate::color::Description::default()
        );
    }

    #[test]
    fn offscreen_needs_an_allocator_and_says_so() {
        use smithay::backend::renderer::Offscreen;

        let Some(mut h) = harness() else { return };
        assert!(!h.renderer.can_allocate());

        let error = Offscreen::<Dmabuf>::create_buffer(
            &mut h.renderer,
            Fourcc::Argb8888,
            (16, 16).into(),
        )
        .expect_err("a renderer without an allocator cannot create buffers");
        assert!(error.to_string().contains("allocator"), "{error}");
    }

    #[test]
    fn a_renderer_with_an_allocator_creates_its_own_targets() {
        use smithay::backend::allocator::Buffer as _;
        use smithay::backend::renderer::Offscreen;

        let Some(TestGpu { device, node }) = require_gpu() else {
            return;
        };
        let Some(allocator) = gbm_allocator(&node) else {
            return;
        };
        let mut renderer =
            VulkanRenderer::with_allocator(&device, allocator).expect("renderer");
        assert!(renderer.can_allocate());

        let mut created =
            Offscreen::<Dmabuf>::create_buffer(&mut renderer, Fourcc::Argb8888, (32, 16).into())
                .expect("create_buffer");
        assert_eq!(created.size().w, 32);
        assert_eq!(created.size().h, 16);

        // And it is a real render target: binding and drawing into it works.
        let mut framebuffer = renderer.bind(&mut created).expect("bind");
        let mut frame = renderer
            .render(&mut framebuffer, (32, 16).into(), Transform::Normal)
            .expect("render");
        frame
            .clear(Color32F::from([0.0, 1.0, 0.0, 1.0]), &all(frame.output_size().w, frame.output_size().h))
            .expect("clear");
        let _ = frame.finish().expect("finish");
    }

    /// The path a screenshot takes: render into an offscreen and read it back
    /// as the same format.
    ///
    /// Asked to read back a format other than the one the target holds, this
    /// renderer refuses — "cannot convert DrmFourcc(AR24) to DrmFourcc(XR24)
    /// while copying" — and every capture on real hardware failed with it,
    /// while the nested GLES renderer converted quietly and hid the mistake.
    #[test]
    fn a_capture_reads_back_the_format_it_rendered() {
        use smithay::backend::renderer::{ExportMem, Offscreen};

        let Some(TestGpu { device, node }) = require_gpu() else {
            return;
        };
        let Some(allocator) = gbm_allocator(&node) else {
            return;
        };
        let mut renderer =
            VulkanRenderer::with_allocator(&device, allocator).expect("renderer");

        let size: smithay::utils::Size<i32, smithay::utils::Buffer> = (32, 16).into();
        let mut target =
            match Offscreen::<Dmabuf>::create_buffer(&mut renderer, Fourcc::Xrgb8888, size) {
                Ok(target) => target,
                Err(_) => {
                    skip("this gpu cannot allocate an XRGB8888 render target");
                    return;
                }
            };

        let mut framebuffer = renderer.bind(&mut target).expect("bind");
        {
            let mut frame = renderer
                .render(&mut framebuffer, (32, 16).into(), Transform::Normal)
                .expect("render");
            frame
                .clear(Color32F::from([0.0, 0.0, 1.0, 1.0]), &all(32, 16))
                .expect("clear");
            let _ = frame.finish().expect("finish");
        }

        let mapping = renderer
            .copy_framebuffer(
                &framebuffer,
                Rectangle::from_size(size),
                Fourcc::Xrgb8888,
            )
            .expect("reading back the same format it was rendered in");
        let pixels = renderer.map_texture(&mapping).expect("map");
        assert_eq!(pixels.len(), 32 * 16 * 4);
        // Blue, as it was cleared: BGRA in memory, so the first byte.
        assert_eq!(pixels[0], 255, "blue channel");
        assert_eq!(pixels[1], 0);
        assert_eq!(pixels[2], 0);

        // And the mistake itself, so it cannot come back quietly: asking for a
        // different format is refused rather than converted.
        assert!(
            renderer
                .copy_framebuffer(&framebuffer, Rectangle::from_size(size), Fourcc::Argb8888)
                .is_err(),
            "a conversion this renderer cannot do was accepted"
        );
    }

    /// Blitting one framebuffer into another, which is what a screencopy that
    /// hands back a dmabuf does.
    #[test]
    fn a_blit_copies_between_framebuffers() {
        use smithay::backend::renderer::Blit;

        let Some(mut h) = harness() else { return };

        let modifier_supports_transfer =
            format::modifiers(h.renderer.device().physical(), Fourcc::Argb8888)
                .into_iter()
                .find(|s| s.modifier == Modifier::Linear)
                .map(|s| s.transfer_src && s.transfer_dst)
                .unwrap_or(false);
        if !modifier_supports_transfer {
            skip("linear ARGB8888 cannot be both copied from and into");
            return;
        }

        // Fill the source by rendering into it.
        let mut source = buffer(&mut h.allocator, 32, 32);
        {
            let mut framebuffer = h.renderer.bind(&mut source).expect("bind source");
            let mut frame = h
                .renderer
                .render(&mut framebuffer, (32, 32).into(), Transform::Normal)
                .expect("render");
            frame
                .clear(Color32F::from([1.0, 0.0, 0.0, 1.0]), &all(frame.output_size().w, frame.output_size().h))
                .expect("clear");
            let _ = frame.finish().expect("finish");
        }

        let mut target = buffer(&mut h.allocator, 32, 32);
        {
            let mut framebuffer = h.renderer.bind(&mut target).expect("bind target");
            let mut frame = h
                .renderer
                .render(&mut framebuffer, (32, 32).into(), Transform::Normal)
                .expect("render");
            frame
                .clear(Color32F::from([0.0, 0.0, 1.0, 1.0]), &all(frame.output_size().w, frame.output_size().h))
                .expect("clear");
            let _ = frame.finish().expect("finish");
        }

        // Blit the top-left quarter of the source into the bottom-right of the
        // target, scaling on the way — the reason to use a blit rather than a
        // copy.
        let from = h.renderer.bind(&mut source).expect("bind from");
        let mut to = h.renderer.bind(&mut target).expect("bind to");
        let sync = h
            .renderer
            .blit(
                &from,
                &mut to,
                Rectangle::new(Point::from((0, 0)), Size::from((16, 16))),
                Rectangle::new(Point::from((16, 16)), Size::from((16, 16))),
                TextureFilter::Linear,
            )
            .expect("blit");
        sync.wait().expect("wait");
        drop(to);
        drop(from);

        // Red where the blit landed, blue everywhere else.
        assert_eq!(pixel(&target, 24, 24), [0, 0, 255, 255], "the blitted region");
        assert_eq!(pixel(&target, 4, 4), [255, 0, 0, 255], "untouched");
        // Non-destructive: the source still holds red.
        assert_eq!(pixel(&source, 4, 4), [0, 0, 255, 255], "the source survived");
    }

    #[test]
    fn a_format_conversion_is_refused_rather_than_producing_wrong_bytes() {
        let Some(mut h) = harness() else { return };
        let mut target = buffer(&mut h.allocator, 16, 16);
        let framebuffer = h.renderer.bind(&mut target).expect("bind");

        let error = h
            .renderer
            .copy_framebuffer(
                &framebuffer,
                Rectangle::new(Point::from((0, 0)), Size::from((16, 16))),
                // The buffer is Argb8888; this would need a shader pass.
                Fourcc::Abgr8888,
            )
            .expect_err("a conversion this renderer cannot do must be refused");
        assert!(error.to_string().contains("convert"), "{error}");
    }

    #[test]
    fn the_advertised_formats_are_ones_that_can_actually_be_imported() {
        // Advertising a format the renderer would reject produces buffers it
        // has to refuse later, which a client experiences as a black window
        // rather than as a negotiation failure.
        let Some(h) = harness() else { return };
        let formats = h.renderer.dmabuf_formats();
        assert!(formats.iter().count() > 0, "no importable formats at all");
        for format in formats.iter() {
            assert!(
                h.renderer.has_dmabuf_format(*format),
                "advertised {format:?} but would not accept it"
            );
        }
    }
}
