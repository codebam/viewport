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
    Bind, Color32F, ContextId, DebugFlags, Frame, ImportDma, Renderer, RendererSuper, Texture,
    TextureFilter,
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
pub struct VulkanTexture(std::sync::Arc<Image>);

impl VulkanTexture {
    pub fn image(&self) -> &Image {
        &self.0
    }
}

impl Texture for VulkanTexture {
    fn width(&self) -> u32 {
        self.0.width()
    }

    fn height(&self) -> u32 {
        self.0.height()
    }

    fn format(&self) -> Option<Fourcc> {
        Some(self.0.fourcc())
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
        })
    }

    pub fn device(&self) -> &Device {
        &self.device
    }

    /// Drop cache entries whose buffer is gone.
    fn reap(&mut self) {
        self.imported.retain(|(weak, _)| !weak.is_gone());
        self.targets.retain(|(weak, _)| !weak.is_gone());
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
        // Only Normal for now. The trait allows a renderer to reject others,
        // and rotating the output means a matrix in the vertex shader rather
        // than the direct pixel mapping it does today. Rejecting is honest;
        // silently ignoring it would put everything in the wrong place.
        if dst_transform != Transform::Normal {
            return Err(Error::Unsupported(format!(
                "output transform {dst_transform:?} is not implemented"
            )));
        }

        VulkanFrame::begin(self, framebuffer, output_size)
    }

    fn wait(&mut self, sync: &SyncPoint) -> Result<(), Self::Error> {
        // A CPU wait. Importing the fence into a Vulkan semaphore and waiting
        // on the queue instead is what VK_KHR_external_semaphore_fd is for,
        // and is the obvious next step.
        sync.wait().map_err(|_| {
            Error::Unsupported("interrupted waiting for a sync point".to_owned())
        })
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

        let texture = VulkanTexture(std::sync::Arc::new(image));
        self.imported.push((dmabuf.weak(), texture.clone()));
        Ok(texture)
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

/// A frame in progress.
pub struct VulkanFrame<'frame, 'buffer> {
    renderer: &'frame mut VulkanRenderer,
    framebuffer: &'frame mut VulkanFramebuffer<'buffer>,
    output_size: Size<i32, Physical>,
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
    fn set_scissor(&self, rects: &[Rectangle<i32, Physical>]) {
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
        let whole = Rectangle::from_size(
            Size::<i32, Physical>::from((
                self.framebuffer.image.width() as i32,
                self.framebuffer.image.height() as i32,
            )),
        );
        let rects: Vec<Rectangle<i32, Physical>> = if at.is_empty() {
            vec![whole]
        } else {
            at.to_vec()
        };
        for rect in rects {
            self.draw_solid(rect, &[], color)?;
        }
        Ok(())
    }

    fn draw_solid(
        &mut self,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        color: Color32F,
    ) -> Result<(), Self::Error> {
        self.set_scissor(damage);

        let target_format = self.framebuffer.image.format();
        let pipeline = self
            .renderer
            .pipelines
            .get(target_format, crate::pipeline::Kind::Solid)?;
        let push = crate::pipeline::Push {
            dst: [
                dst.loc.x as f32,
                dst.loc.y as f32,
                dst.size.w as f32,
                dst.size.h as f32,
            ],
            src: [0.0, 0.0, 1.0, 1.0],
            // Smithay's Color32F is already premultiplied.
            color: [color.r(), color.g(), color.b(), color.a()],
            target: [
                self.framebuffer.image.width() as f32,
                self.framebuffer.image.height() as f32,
            ],
            alpha: 1.0,
        };

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
            handle.cmd_draw(buffer, 4, 1, 0, 0);
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
        if src_transform != Transform::Normal {
            return Err(Error::Unsupported(format!(
                "surface transform {src_transform:?} is not implemented"
            )));
        }

        self.set_scissor(damage);

        // src arrives in buffer pixels; the shader wants normalised
        // coordinates, which is the only place the texture's own size is
        // needed.
        let (tw, th) = (texture.width() as f64, texture.height() as f64);
        let push = crate::pipeline::Push {
            dst: [
                dst.loc.x as f32,
                dst.loc.y as f32,
                dst.size.w as f32,
                dst.size.h as f32,
            ],
            src: [
                (src.loc.x / tw) as f32,
                (src.loc.y / th) as f32,
                (src.size.w / tw) as f32,
                (src.size.h / th) as f32,
            ],
            color: [1.0, 1.0, 1.0, 1.0],
            target: [
                self.framebuffer.image.width() as f32,
                self.framebuffer.image.height() as f32,
            ],
            alpha,
        };

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
            .image_view(texture.0.view())
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
            handle.cmd_draw(buffer, 4, 1, 0, 0);
        }

        self.set_scissor(&[]);
        Ok(())
    }

    fn transformation(&self) -> Transform {
        Transform::Normal
    }

    fn output_size(&self) -> Size<i32, Physical> {
        self.output_size
    }

    fn wait(&mut self, sync: &SyncPoint) -> Result<(), Self::Error> {
        sync.wait()
            .map_err(|_| Error::Unsupported("interrupted waiting for a sync point".to_owned()))
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

        // A CPU wait, returning an already-signalled point.
        //
        // The honest version exports a fence fd from the submission and hands
        // it back as the SyncPoint, so the caller can pass it to KMS instead
        // of blocking. That needs VK_KHR_external_semaphore_fd wired into the
        // submit, and it is the next thing this crate should grow.
        self.renderer
            .commands
            .wait(std::time::Duration::from_secs(5))?;
        Ok(SyncPoint::signaled())
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
            .clear(Color32F::from([0.0, 0.0, 0.0, 1.0]), &[])
            .expect("clear");
        frame
            .draw_solid(
                Rectangle::new(Point::from((0, 0)), Size::from((32, 32))),
                &[],
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
            .clear(Color32F::from([0.0, 0.0, 0.0, 1.0]), &[])
            .expect("clear");
        frame
            .render_texture_from_to(
                &texture,
                Rectangle::from_size(Size::from((32.0, 32.0))),
                Rectangle::new(Point::from((16, 16)), Size::from((32, 32))),
                &[],
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

    #[test]
    fn importing_the_same_buffer_twice_reuses_the_image() {
        // Without this the renderer would allocate a fresh VkImage and a fresh
        // memory import for every surface, every frame.
        let Some(mut h) = harness() else { return };
        let source = buffer(&mut h.allocator, 16, 16);

        let first = h.renderer.import_dmabuf(&source, None).expect("first");
        let second = h.renderer.import_dmabuf(&source, None).expect("second");

        assert!(
            std::sync::Arc::ptr_eq(&first.0, &second.0),
            "the second import allocated a second image"
        );
    }

    #[test]
    fn a_rotated_output_is_refused_rather_than_drawn_wrong() {
        let Some(mut h) = harness() else { return };
        let mut target = buffer(&mut h.allocator, 32, 32);
        let mut framebuffer = h.renderer.bind(&mut target).expect("bind");

        let error = h
            .renderer
            .render(&mut framebuffer, (32, 32).into(), Transform::_90)
            .err()
            .expect("a transform this renderer cannot apply must be refused");
        assert!(error.to_string().contains("transform"), "{error}");
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
