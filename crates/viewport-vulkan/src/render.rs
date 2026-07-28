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
use crate::Device;

/// A colour, in the same order Vulkan wants it: red, green, blue, alpha.
///
/// Note this is the *format's* component order, not the byte order in memory.
/// `B8G8R8A8_UNORM` still calls its red component red; the name describes
/// where the bytes sit, which is why a red clear lands in byte 2.
pub type Color = [f32; 4];

/// A frame being recorded into a render target.
///
/// Held by value and consumed by [`Frame::finish`], so a frame cannot be
/// forgotten half-recorded.
pub struct Frame<'a> {
    device: Device,
    commands: &'a mut Commands,
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
    /// The clear is part of beginning rather than a separate command because
    /// `LOAD_OP_CLEAR` is free where a explicit clear is not: the tiler
    /// discards the previous contents instead of reading them in.
    pub fn begin(
        device: &Device,
        commands: &'a mut Commands,
        target: &'a Image,
        clear: Option<Color>,
    ) -> Result<Self> {
        anyhow::ensure!(
            target.purpose() == Purpose::Render,
            "{target:?} was imported for sampling, not rendering"
        );

        let buffer = commands.begin()?;
        let handle = device.handle();

        // Claim the image before touching it. Until this runs it belongs to
        // whoever allocated it.
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
            // Nothing is preserved across frames in a compositor that redraws
            // damaged regions, but a partial redraw needs what was there.
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
            device.dynamic_rendering().cmd_begin_rendering(buffer, &rendering);

            // Viewport and scissor are dynamic state, so they have to be set
            // even when they cover the whole target.
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
            target,
            buffer,
        })
    }

    pub fn command_buffer(&self) -> vk::CommandBuffer {
        self.buffer
    }

    /// End rendering, release the image, and submit.
    ///
    /// Does not wait. The caller either exports a fence or calls
    /// [`Commands::wait`]; blocking here would defeat the point of the whole
    /// explicit-sync path.
    pub fn finish(self) -> Result<()> {
        let handle = self.device.handle();
        unsafe {
            self.device.dynamic_rendering().cmd_end_rendering(self.buffer);

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
    target: &Image,
    color: Color,
) -> Result<()> {
    Frame::begin(device, commands, target, Some(color))?.finish()?;
    commands.wait(Duration::from_secs(5))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format;
    use crate::test_support::{gbm_allocator, require_gpu, TestGpu};

    use smithay::backend::allocator::dmabuf::{AsDmabuf, DmabufMappingMode, DmabufSyncFlags};
    use smithay::backend::allocator::{Allocator, Fourcc, Modifier};

    /// Render into a DMA-BUF with Vulkan, then read it back on the CPU.
    ///
    /// A linear buffer so the mapping is interpretable without knowing the
    /// tiling — the point here is whether the render path works, not whether
    /// this file can decode a vendor swizzle.
    #[test]
    fn a_clear_reaches_the_buffer() {
        let Some(TestGpu { device, node }) = require_gpu() else {
            return;
        };
        let Some(mut allocator) = gbm_allocator(&node) else {
            return;
        };

        let renderable_linear = format::modifiers(device.physical(), Fourcc::Argb8888)
            .into_iter()
            .any(|s| s.modifier == Modifier::Linear && s.rendering);
        if !renderable_linear {
            crate::test_support::skip("no renderable linear ARGB8888");
            return;
        }

        let dmabuf = allocator
            .create_buffer(64, 64, Fourcc::Argb8888, &[Modifier::Linear])
            .expect("gbm allocation")
            .export()
            .expect("export");

        let target = Image::import(&device, &dmabuf, Purpose::Render).expect("import");
        let mut commands = Commands::new(&device).expect("commands");

        // Pure red, fully opaque.
        clear_and_wait(&device, &mut commands, &target, [1.0, 0.0, 0.0, 1.0]).expect("clear");

        // The ioctl that makes the GPU's writes visible to this mapping.
        dmabuf
            .sync_plane(0, DmabufSyncFlags::START | DmabufSyncFlags::READ)
            .expect("dmabuf sync start");
        let mapping = dmabuf
            .map_plane(0, DmabufMappingMode::READ)
            .expect("map plane");

        let stride = dmabuf.strides().next().expect("a stride") as usize;
        let pixels =
            unsafe { std::slice::from_raw_parts(mapping.ptr() as *const u8, mapping.length()) };

        // ARGB8888 is B, G, R, A in memory, so a red clear is byte 2.
        let check = |x: usize, y: usize| {
            let at = y * stride + x * 4;
            [pixels[at], pixels[at + 1], pixels[at + 2], pixels[at + 3]]
        };
        for (x, y) in [(0, 0), (63, 0), (0, 63), (63, 63), (32, 32)] {
            assert_eq!(
                check(x, y),
                [0, 0, 255, 255],
                "pixel at {x},{y} is not red"
            );
        }

        drop(mapping);
        dmabuf
            .sync_plane(0, DmabufSyncFlags::END | DmabufSyncFlags::READ)
            .expect("dmabuf sync end");
    }

    #[test]
    fn a_second_clear_overwrites_the_first() {
        // Proves the command buffer is genuinely re-recorded and resubmitted,
        // rather than the first result being left in place.
        let Some(TestGpu { device, node }) = require_gpu() else {
            return;
        };
        let Some(mut allocator) = gbm_allocator(&node) else {
            return;
        };
        if !format::modifiers(device.physical(), Fourcc::Argb8888)
            .into_iter()
            .any(|s| s.modifier == Modifier::Linear && s.rendering)
        {
            crate::test_support::skip("no renderable linear ARGB8888");
            return;
        }

        let dmabuf = allocator
            .create_buffer(32, 32, Fourcc::Argb8888, &[Modifier::Linear])
            .expect("gbm allocation")
            .export()
            .expect("export");
        let target = Image::import(&device, &dmabuf, Purpose::Render).expect("import");
        let mut commands = Commands::new(&device).expect("commands");

        clear_and_wait(&device, &mut commands, &target, [1.0, 0.0, 0.0, 1.0]).expect("red");
        clear_and_wait(&device, &mut commands, &target, [0.0, 0.0, 1.0, 1.0]).expect("blue");

        dmabuf
            .sync_plane(0, DmabufSyncFlags::START | DmabufSyncFlags::READ)
            .expect("sync start");
        let mapping = dmabuf.map_plane(0, DmabufMappingMode::READ).expect("map");
        let pixels =
            unsafe { std::slice::from_raw_parts(mapping.ptr() as *const u8, mapping.length()) };

        // Blue is byte 0.
        assert_eq!(&pixels[0..4], &[255, 0, 0, 255], "the second clear did not land");

        drop(mapping);
        let _ = dmabuf.sync_plane(0, DmabufSyncFlags::END | DmabufSyncFlags::READ);
    }

    #[test]
    fn a_sampling_image_cannot_be_rendered_into() {
        let Some(TestGpu { device, node }) = require_gpu() else {
            return;
        };
        let Some(mut allocator) = gbm_allocator(&node) else {
            return;
        };
        let sampleable: Vec<_> = format::modifiers(device.physical(), Fourcc::Argb8888)
            .into_iter()
            .filter(|s| s.sampling && s.planes == 1)
            .map(|s| s.modifier)
            .collect();
        if sampleable.is_empty() {
            crate::test_support::skip("no sampleable ARGB8888 modifier");
            return;
        }

        let dmabuf = allocator
            .create_buffer(32, 32, Fourcc::Argb8888, &sampleable)
            .expect("gbm allocation")
            .export()
            .expect("export");
        let image = Image::import(&device, &dmabuf, Purpose::Sample).expect("import");
        let mut commands = Commands::new(&device).expect("commands");

        // The image lacks COLOR_ATTACHMENT usage, so this would be a validation
        // error rather than a Rust error if it were not caught here.
        let error = Frame::begin(&device, &mut commands, &image, Some([0.0; 4]))
            .expect_err("a sampled image is not a render target");
        assert!(
            error.to_string().contains("imported for sampling"),
            "unexpected error: {error}"
        );
    }
}
