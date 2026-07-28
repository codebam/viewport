// SPDX-License-Identifier: MIT
//
// Recording a frame.
//
// Uses dynamic rendering rather than VkRenderPass and VkFramebuffer. For a
// compositor that is pure subtraction: every frame targets a different
// imported image, so a cache of render pass objects keyed by format would be
// rebuilt constantly and buy nothing.

use std::time::Duration;

use anyhow::Result;
use ash::vk;

use crate::command::Commands;
use crate::image::{Image, Purpose};
use crate::pipeline::{Kind, Pipelines, Push};
use crate::Device;

/// A colour, in the same order Vulkan wants it: red, green, blue, alpha.
///
/// Note this is the *format's* component order, not the byte order in memory.
/// `B8G8R8A8_UNORM` still calls its red component red; the name describes
/// where the bytes sit, which is why a red clear lands in byte 2.
///
/// Premultiplied, like everything else here.
pub type Color = [f32; 4];

/// A rectangle in target pixels: x, y, width, height.
pub type Rect = [f32; 4];

/// A frame being recorded into a render target.
///
/// Consumed by [`Frame::finish`], so a frame cannot be left half-recorded.
pub struct Frame<'a> {
    device: Device,
    commands: &'a mut Commands,
    pipelines: &'a mut Pipelines,
    target: &'a Image,
    buffer: vk::CommandBuffer,
}

impl std::fmt::Debug for Frame<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Frame").field("target", self.target).finish()
    }
}

impl<'a> Frame<'a> {
    /// Begin rendering into `target`, clearing it to `clear`.
    ///
    /// Every image that will be sampled during this frame has to be named in
    /// `sources`. Layout transitions and queue ownership transfers are not
    /// allowed inside a render pass, so a texture discovered mid-frame is one
    /// that cannot legally be acquired — hence the up-front list rather than a
    /// transition inside `draw_texture`.
    ///
    /// The clear is part of beginning rather than a separate command because
    /// `LOAD_OP_CLEAR` lets a tiler discard the previous contents instead of
    /// reading them in.
    pub fn begin(
        device: &Device,
        commands: &'a mut Commands,
        pipelines: &'a mut Pipelines,
        target: &'a Image,
        clear: Option<Color>,
        sources: &[&Image],
    ) -> Result<Self> {
        anyhow::ensure!(
            target.purpose() == Purpose::Render,
            "{target:?} was imported for sampling, not rendering"
        );

        let buffer = commands.begin()?;
        let handle = device.handle();

        // Claim everything before touching it. Until these run the images
        // belong to whoever allocated them.
        let mut barriers =
            Vec::with_capacity(sources.len() + 1);
        barriers.push(target.acquire_barrier(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL));
        for source in sources {
            anyhow::ensure!(
                source.purpose() == Purpose::Sample,
                "{source:?} was imported for rendering, not sampling"
            );
            barriers.push(source.acquire_barrier(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL));
        }

        unsafe {
            handle.cmd_pipeline_barrier(
                buffer,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT
                    | vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &barriers,
            );
        }

        let mut attachment = vk::RenderingAttachmentInfo::default()
            .image_view(target.view())
            .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .store_op(vk::AttachmentStoreOp::STORE);
        attachment = match clear {
            Some(color) => attachment
                .load_op(vk::AttachmentLoadOp::CLEAR)
                .clear_value(vk::ClearValue {
                    color: vk::ClearColorValue { float32: color },
                }),
            // A partial redraw needs what was already there.
            None => attachment.load_op(vk::AttachmentLoadOp::LOAD),
        };

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

            // Dynamic state, so it has to be set even covering the whole target.
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
            device: device.clone(),
            commands,
            pipelines,
            target,
            buffer,
        })
    }

    pub fn command_buffer(&self) -> vk::CommandBuffer {
        self.buffer
    }

    fn push(&self, dst: Rect, src: Rect, color: Color, alpha: f32) -> Push {
        Push {
            dst,
            src,
            color,
            target: [self.target.width() as f32, self.target.height() as f32],
            alpha,
        }
    }

    /// Fill a rectangle with a flat colour.
    pub fn draw_solid(&mut self, dst: Rect, color: Color, alpha: f32) -> Result<()> {
        let pipeline = self.pipelines.get(self.target.format(), Kind::Solid)?;
        let push = self.push(dst, [0.0, 0.0, 1.0, 1.0], color, alpha);
        let handle = self.device.handle();

        unsafe {
            handle.cmd_bind_pipeline(self.buffer, vk::PipelineBindPoint::GRAPHICS, pipeline);
            handle.cmd_push_constants(
                self.buffer,
                self.pipelines.layout(),
                vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                0,
                push.as_bytes(),
            );
            // Four vertices, one triangle strip, one quad. No buffers.
            handle.cmd_draw(self.buffer, 4, 1, 0, 0);
        }
        Ok(())
    }

    /// Draw a surface.
    ///
    /// `texture` must have been named in `sources` when the frame began;
    /// otherwise it is still owned by the foreign queue and its contents are
    /// undefined here.
    pub fn draw_texture(
        &mut self,
        dst: Rect,
        src: Rect,
        texture: &Image,
        alpha: f32,
    ) -> Result<()> {
        let pipeline = self.pipelines.get(self.target.format(), Kind::Texture)?;
        // White tint: the texture's own colours, scaled by alpha.
        let push = self.push(dst, src, [1.0, 1.0, 1.0, 1.0], alpha);
        let handle = self.device.handle();

        let image_info = vk::DescriptorImageInfo::default()
            .sampler(self.pipelines.sampler())
            .image_view(texture.view())
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
        let infos = [image_info];
        let write = vk::WriteDescriptorSet::default()
            .dst_binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .image_info(&infos);

        unsafe {
            handle.cmd_bind_pipeline(self.buffer, vk::PipelineBindPoint::GRAPHICS, pipeline);
            // Pushed straight into the command buffer: no pool, no allocation,
            // no recycling between frames.
            self.device.push_descriptor().cmd_push_descriptor_set(
                self.buffer,
                vk::PipelineBindPoint::GRAPHICS,
                self.pipelines.layout(),
                0,
                &[write],
            );
            handle.cmd_push_constants(
                self.buffer,
                self.pipelines.layout(),
                vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                0,
                push.as_bytes(),
            );
            handle.cmd_draw(self.buffer, 4, 1, 0, 0);
        }
        Ok(())
    }

    /// End rendering, release the target, and submit.
    ///
    /// Does not wait. The caller either exports a fence or calls
    /// [`Commands::wait`]; blocking here would defeat the point of the whole
    /// explicit-sync path.
    pub fn finish(self) -> Result<()> {
        let handle = self.device.handle();
        unsafe {
            self.device
                .dynamic_rendering()
                .cmd_end_rendering(self.buffer);

            // Hand the image on. Whoever reads it next — KMS, another API, a
            // CPU mapping — is not this queue family, and without this its
            // contents are undefined for them.
            let release = self
                .target
                .release_barrier(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL);
            handle.cmd_pipeline_barrier(
                self.buffer,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[release],
            );
        }
        self.commands.submit()
    }
}

/// Clear a render target and wait for it.
///
/// A convenience for tests and for the "nothing to draw" case. Real frames go
/// through [`Frame`] and do not wait.
pub fn clear_and_wait(
    device: &Device,
    commands: &mut Commands,
    pipelines: &mut Pipelines,
    target: &Image,
    color: Color,
) -> Result<()> {
    Frame::begin(device, commands, pipelines, target, Some(color), &[])?.finish()?;
    commands.wait(Duration::from_secs(5))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format;
    use crate::test_support::{gbm_allocator, require_gpu, skip, TestGpu};

    use smithay::backend::allocator::dmabuf::{
        AsDmabuf, Dmabuf, DmabufMappingMode, DmabufSyncFlags,
    };
    use smithay::backend::allocator::{Allocator, Fourcc, Modifier};

    struct Harness {
        device: Device,
        commands: Commands,
        pipelines: Pipelines,
        allocator: smithay::backend::allocator::gbm::GbmAllocator<std::fs::File>,
    }

    /// Everything the drawing tests need, or `None` where this machine cannot
    /// answer the question.
    fn harness() -> Option<Harness> {
        let TestGpu { device, node } = require_gpu()?;
        let allocator = gbm_allocator(&node)?;

        // Linear so a CPU mapping is interpretable without decoding a vendor
        // swizzle. The question here is whether drawing works, not tiling.
        let linear_renderable = format::modifiers(device.physical(), Fourcc::Argb8888)
            .into_iter()
            .any(|s| s.modifier == Modifier::Linear && s.rendering);
        let linear_sampleable = format::modifiers(device.physical(), Fourcc::Argb8888)
            .into_iter()
            .any(|s| s.modifier == Modifier::Linear && s.sampling);
        if !linear_renderable || !linear_sampleable {
            skip("no linear ARGB8888 that is both renderable and sampleable");
            return None;
        }

        let commands = Commands::new(&device).expect("commands");
        let pipelines = Pipelines::new(&device).expect("pipelines");
        Some(Harness {
            device,
            commands,
            pipelines,
            allocator,
        })
    }

    fn linear_buffer(
        allocator: &mut smithay::backend::allocator::gbm::GbmAllocator<std::fs::File>,
        width: u32,
        height: u32,
    ) -> Dmabuf {
        allocator
            .create_buffer(width, height, Fourcc::Argb8888, &[Modifier::Linear])
            .expect("gbm allocation")
            .export()
            .expect("export")
    }

    /// Read one pixel as it sits in memory: B, G, R, A.
    fn pixel(buffer: &Dmabuf, x: usize, y: usize) -> [u8; 4] {
        buffer
            .sync_plane(0, DmabufSyncFlags::START | DmabufSyncFlags::READ)
            .expect("sync start");
        let mapping = buffer.map_plane(0, DmabufMappingMode::READ).expect("map");
        let stride = buffer.strides().next().expect("stride") as usize;
        let bytes =
            unsafe { std::slice::from_raw_parts(mapping.ptr() as *const u8, mapping.length()) };
        let at = y * stride + x * 4;
        let out = [bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]];
        drop(mapping);
        let _ = buffer.sync_plane(0, DmabufSyncFlags::END | DmabufSyncFlags::READ);
        out
    }

    /// Fill a buffer on the CPU, to give a texture known contents.
    fn fill(buffer: &Dmabuf, color: [u8; 4], width: usize, height: usize) {
        buffer
            .sync_plane(0, DmabufSyncFlags::START | DmabufSyncFlags::WRITE)
            .expect("sync start");
        let mapping = buffer
            .map_plane(0, DmabufMappingMode::WRITE)
            .expect("map write");
        let stride = buffer.strides().next().expect("stride") as usize;
        let bytes =
            unsafe { std::slice::from_raw_parts_mut(mapping.ptr() as *mut u8, mapping.length()) };
        for y in 0..height {
            for x in 0..width {
                let at = y * stride + x * 4;
                bytes[at..at + 4].copy_from_slice(&color);
            }
        }
        drop(mapping);
        let _ = buffer.sync_plane(0, DmabufSyncFlags::END | DmabufSyncFlags::WRITE);
    }

    #[test]
    fn a_clear_reaches_the_buffer() {
        let Some(mut h) = harness() else { return };
        let dmabuf = linear_buffer(&mut h.allocator, 64, 64);
        let target = Image::import(&h.device, &dmabuf, Purpose::Render).expect("import");

        clear_and_wait(
            &h.device,
            &mut h.commands,
            &mut h.pipelines,
            &target,
            [1.0, 0.0, 0.0, 1.0],
        )
        .expect("clear");

        // ARGB8888 is B, G, R, A in memory, so a red clear is byte 2.
        for (x, y) in [(0, 0), (63, 0), (0, 63), (63, 63), (32, 32)] {
            assert_eq!(pixel(&dmabuf, x, y), [0, 0, 255, 255], "pixel {x},{y}");
        }
    }

    #[test]
    fn a_second_clear_overwrites_the_first() {
        // Proves the command buffer is genuinely re-recorded rather than the
        // first result being left in place.
        let Some(mut h) = harness() else { return };
        let dmabuf = linear_buffer(&mut h.allocator, 32, 32);
        let target = Image::import(&h.device, &dmabuf, Purpose::Render).expect("import");

        for color in [[1.0, 0.0, 0.0, 1.0], [0.0, 0.0, 1.0, 1.0]] {
            clear_and_wait(
                &h.device,
                &mut h.commands,
                &mut h.pipelines,
                &target,
                color,
            )
            .expect("clear");
        }

        // Blue is byte 0.
        assert_eq!(pixel(&dmabuf, 0, 0), [255, 0, 0, 255]);
    }

    #[test]
    fn a_solid_quad_lands_where_it_was_put() {
        let Some(mut h) = harness() else { return };
        let dmabuf = linear_buffer(&mut h.allocator, 64, 64);
        let target = Image::import(&h.device, &dmabuf, Purpose::Render).expect("import");

        // Black background, opaque green square over the top-left quadrant.
        let mut frame = Frame::begin(
            &h.device,
            &mut h.commands,
            &mut h.pipelines,
            &target,
            Some([0.0, 0.0, 0.0, 1.0]),
            &[],
        )
        .expect("begin");
        frame
            .draw_solid([0.0, 0.0, 32.0, 32.0], [0.0, 1.0, 0.0, 1.0], 1.0)
            .expect("draw");
        frame.finish().expect("finish");
        h.commands.wait(Duration::from_secs(5)).expect("wait");

        // Green is byte 1. Inside the quad and outside it, including a pixel
        // either side of each edge — a quad that is off by one or flipped
        // vertically fails here rather than looking plausible.
        assert_eq!(pixel(&dmabuf, 0, 0), [0, 255, 0, 255], "top-left corner");
        assert_eq!(pixel(&dmabuf, 31, 31), [0, 255, 0, 255], "inside, far corner");
        assert_eq!(pixel(&dmabuf, 32, 0), [0, 0, 0, 255], "just right of the quad");
        assert_eq!(pixel(&dmabuf, 0, 32), [0, 0, 0, 255], "just below the quad");
        assert_eq!(pixel(&dmabuf, 63, 63), [0, 0, 0, 255], "opposite corner");
    }

    #[test]
    fn a_texture_is_sampled_into_the_target() {
        let Some(mut h) = harness() else { return };

        // A source buffer filled on the CPU with an unmistakable colour.
        let source_buffer = linear_buffer(&mut h.allocator, 32, 32);
        // B, G, R, A in memory: pure blue, opaque.
        fill(&source_buffer, [255, 0, 0, 255], 32, 32);
        let source = Image::import(&h.device, &source_buffer, Purpose::Sample).expect("import src");

        let dmabuf = linear_buffer(&mut h.allocator, 64, 64);
        let target = Image::import(&h.device, &dmabuf, Purpose::Render).expect("import dst");

        let mut frame = Frame::begin(
            &h.device,
            &mut h.commands,
            &mut h.pipelines,
            &target,
            Some([0.0, 0.0, 0.0, 1.0]),
            &[&source],
        )
        .expect("begin");
        frame
            .draw_texture(
                [16.0, 16.0, 32.0, 32.0],
                [0.0, 0.0, 1.0, 1.0],
                &source,
                1.0,
            )
            .expect("draw");
        frame.finish().expect("finish");
        h.commands.wait(Duration::from_secs(5)).expect("wait");

        // Blue where the texture landed, black outside it.
        assert_eq!(pixel(&dmabuf, 32, 32), [255, 0, 0, 255], "middle of the texture");
        assert_eq!(pixel(&dmabuf, 17, 17), [255, 0, 0, 255], "inside, near corner");
        assert_eq!(pixel(&dmabuf, 8, 8), [0, 0, 0, 255], "above-left of it");
        assert_eq!(pixel(&dmabuf, 60, 60), [0, 0, 0, 255], "below-right of it");
    }

    #[test]
    fn a_sampling_image_cannot_be_rendered_into() {
        let Some(mut h) = harness() else { return };
        let dmabuf = linear_buffer(&mut h.allocator, 32, 32);
        let image = Image::import(&h.device, &dmabuf, Purpose::Sample).expect("import");

        // The image lacks COLOR_ATTACHMENT usage, so without this check it
        // would be a validation error rather than a Rust one.
        let error = Frame::begin(
            &h.device,
            &mut h.commands,
            &mut h.pipelines,
            &image,
            Some([0.0; 4]),
            &[],
        )
        .expect_err("a sampled image is not a render target");
        assert!(
            error.to_string().contains("imported for sampling"),
            "unexpected error: {error}"
        );
    }
}
