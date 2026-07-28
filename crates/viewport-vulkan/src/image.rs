// SPDX-License-Identifier: MIT
//
// Vulkan images over DMA-BUFs.
//
// This is the point of the whole crate: a client hands the compositor a
// DMA-BUF, and it has to become something a shader can sample without anyone
// copying pixels. The import is not just "wrap the fd" — the image has to be
// created with the *same* memory layout the allocator used, which is what the
// modifier and the per-plane offsets and pitches describe. Create it with the
// wrong tiling and the driver will happily read the buffer as garbage.

use std::os::fd::{AsRawFd, OwnedFd};

use anyhow::{anyhow, Context as _, Result};
use ash::vk;
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::allocator::Buffer as _;

use crate::format;
use crate::Device;

/// The external memory handle type every DMA-BUF import uses.
const HANDLE_TYPE: vk::ExternalMemoryHandleTypeFlags =
    vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT;

/// What an image is for, which decides its usage flags and initial layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// A client buffer or the shell's buffer: sampled by the compositor.
    Sample,
    /// An output buffer: rendered into, then scanned out.
    Render,
}

impl Purpose {
    fn usage(self) -> vk::ImageUsageFlags {
        match self {
            Self::Sample => vk::ImageUsageFlags::SAMPLED,
            // TRANSFER_SRC so a screenshot or a copy-capture request can read
            // the output back without a second render.
            Self::Render => {
                vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC
            }
        }
    }
}

/// A Vulkan image backed by an imported DMA-BUF.
pub struct Image {
    device: Device,
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,

    width: u32,
    height: u32,
    format: vk::Format,
    /// The DRM format it was imported as. Kept because `vk::Format` cannot
    /// distinguish XRGB from ARGB, and Smithay's `Texture` trait asks for the
    /// fourcc rather than the Vulkan format.
    fourcc: smithay::backend::allocator::Fourcc,
    has_alpha: bool,
    purpose: Purpose,

    /// Images arrive from outside this device's queue family, and the first
    /// barrier has to say so. Tracked because it is only true once: after the
    /// first acquire the image belongs to us.
    foreign: std::cell::Cell<bool>,
}

impl Image {
    /// Import a DMA-BUF.
    ///
    /// The buffer's fds are duplicated: `vkAllocateMemory` takes ownership of
    /// the fd it is given on success, and the caller's `Dmabuf` still owns
    /// its own copies.
    pub fn import(device: &Device, buffer: &Dmabuf, purpose: Purpose) -> Result<Self> {
        let fourcc = buffer.format().code;
        let modifier = u64::from(buffer.format().modifier);
        let size = buffer.size();
        let (width, height) = (size.w as u32, size.h as u32);

        let vk_format = format::to_vulkan(fourcc)
            .ok_or_else(|| anyhow!("no Vulkan format for {fourcc:?}"))?;

        // Refuse a modifier the device did not advertise rather than letting
        // vkCreateImage fail with something less informative.
        let supported = format::modifiers(device.physical(), fourcc);
        let support = supported
            .iter()
            .find(|s| u64::from(s.modifier) == modifier)
            .ok_or_else(|| {
                anyhow!("{} does not support {fourcc:?} with modifier {modifier:#x}", device.name())
            })?;
        match purpose {
            Purpose::Sample if !support.sampling => {
                return Err(anyhow!("{fourcc:?} modifier {modifier:#x} cannot be sampled"))
            }
            Purpose::Render if !support.rendering => {
                return Err(anyhow!(
                    "{fourcc:?} modifier {modifier:#x} cannot be rendered into"
                ))
            }
            _ => {}
        }

        let planes = buffer.num_planes();
        if planes as u32 != support.planes {
            return Err(anyhow!(
                "buffer has {planes} plane(s), modifier {modifier:#x} describes {}",
                support.planes
            ));
        }

        // The layout the allocator actually used. `array_pitch` and
        // `depth_pitch` are zero because these are 2D single-layer images;
        // `size` must be zero on import, where the driver derives it.
        let layouts: Vec<vk::SubresourceLayout> = buffer
            .offsets()
            .zip(buffer.strides())
            .map(|(offset, stride)| vk::SubresourceLayout {
                offset: offset as u64,
                size: 0,
                row_pitch: stride as u64,
                array_pitch: 0,
                depth_pitch: 0,
            })
            .collect();

        let mut modifier_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
            .drm_format_modifier(modifier)
            .plane_layouts(&layouts);
        let mut external = vk::ExternalMemoryImageCreateInfo::default().handle_types(HANDLE_TYPE);

        let create_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk_format)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            // Not OPTIMAL or LINEAR: the layout is whatever the modifier says.
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(purpose.usage())
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            // UNDEFINED would discard the contents, which for an imported
            // client buffer is exactly the pixels we were given.
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut modifier_info)
            .push_next(&mut external);

        let handle = device.handle();
        let image = unsafe { handle.create_image(&create_info, None) }.context("vkCreateImage")?;

        // From here on every early return has to clean up what came before it.
        let result = (|| -> Result<(vk::DeviceMemory, vk::ImageView)> {
            let requirements = unsafe { handle.get_image_memory_requirements(image) };

            // The driver says which memory types this specific fd can back.
            let plane_fd = buffer
                .handles()
                .next()
                .ok_or_else(|| anyhow!("dmabuf has no planes"))?;
            let mut fd_properties = vk::MemoryFdPropertiesKHR::default();
            unsafe {
                device.external_memory_fd().get_memory_fd_properties(
                    HANDLE_TYPE,
                    plane_fd.as_raw_fd(),
                    &mut fd_properties,
                )
            }
            .context("vkGetMemoryFdPropertiesKHR")?;

            let memory_type = device
                .memory_type(requirements.memory_type_bits, fd_properties.memory_type_bits)
                .ok_or_else(|| anyhow!("no memory type can back this dmabuf"))?;

            // vkAllocateMemory consumes the fd on success, so hand it a copy.
            let owned: OwnedFd = plane_fd.try_clone_to_owned().context("dup dmabuf fd")?;

            let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
            let mut import = vk::ImportMemoryFdInfoKHR::default()
                .handle_type(HANDLE_TYPE)
                .fd(owned.as_raw_fd());
            let allocate = vk::MemoryAllocateInfo::default()
                .allocation_size(requirements.size)
                .memory_type_index(memory_type)
                .push_next(&mut dedicated)
                .push_next(&mut import);

            let memory =
                unsafe { handle.allocate_memory(&allocate, None) }.context("vkAllocateMemory")?;
            // Ownership passed to Vulkan; dropping it here would close a fd
            // the driver still holds.
            std::mem::forget(owned);

            if let Err(e) = unsafe { handle.bind_image_memory(image, memory, 0) } {
                unsafe { handle.free_memory(memory, None) };
                return Err(anyhow::Error::from(e).context("vkBindImageMemory"));
            }

            let view_info = vk::ImageViewCreateInfo::default()
                .image(image)
                .view_type(vk::ImageViewType::TYPE_2D)
                .format(vk_format)
                .subresource_range(vk::ImageSubresourceRange {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: 0,
                    layer_count: 1,
                });
            let view = match unsafe { handle.create_image_view(&view_info, None) } {
                Ok(view) => view,
                Err(e) => {
                    unsafe { handle.free_memory(memory, None) };
                    return Err(anyhow::Error::from(e).context("vkCreateImageView"));
                }
            };

            Ok((memory, view))
        })();

        let (memory, view) = match result {
            Ok(pair) => pair,
            Err(e) => {
                unsafe { handle.destroy_image(image, None) };
                return Err(e);
            }
        };

        Ok(Self {
            device: device.clone(),
            image,
            memory,
            view,
            width,
            height,
            format: vk_format,
            fourcc,
            has_alpha: format::has_alpha(fourcc),
            purpose,
            foreign: std::cell::Cell::new(true),
        })
    }

    pub fn handle(&self) -> vk::Image {
        self.image
    }

    pub fn view(&self) -> vk::ImageView {
        self.view
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn format(&self) -> vk::Format {
        self.format
    }

    pub fn fourcc(&self) -> smithay::backend::allocator::Fourcc {
        self.fourcc
    }

    pub fn has_alpha(&self) -> bool {
        self.has_alpha
    }

    pub fn purpose(&self) -> Purpose {
        self.purpose
    }

    /// A barrier handing the image back to whoever will consume it next.
    ///
    /// After rendering, the image goes to KMS, to another API, or to a CPU
    /// mapping — none of which are this queue family. Releasing it to
    /// `VK_QUEUE_FAMILY_FOREIGN_EXT` in `GENERAL` layout is what makes the
    /// contents defined for all of them.
    pub fn release_barrier(&self, from: vk::ImageLayout) -> vk::ImageMemoryBarrier<'static> {
        vk::ImageMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
            .dst_access_mask(vk::AccessFlags::empty())
            .old_layout(from)
            .new_layout(vk::ImageLayout::GENERAL)
            .src_queue_family_index(self.device.queue_family())
            .dst_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
            .image(self.image)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            })
    }

    /// A barrier taking ownership of the image from whoever produced it.
    ///
    /// An imported buffer belongs to `VK_QUEUE_FAMILY_FOREIGN_EXT` until this
    /// runs. Skipping it is the kind of mistake that works on one driver and
    /// corrupts on another, because nothing checks it.
    pub fn acquire_barrier(&self, layout: vk::ImageLayout) -> vk::ImageMemoryBarrier<'static> {
        let src_family = if self.foreign.replace(false) {
            vk::QUEUE_FAMILY_FOREIGN_EXT
        } else {
            self.device.queue_family()
        };

        vk::ImageMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::empty())
            .dst_access_mask(match self.purpose {
                Purpose::Sample => vk::AccessFlags::SHADER_READ,
                Purpose::Render => vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
            })
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(layout)
            .src_queue_family_index(src_family)
            .dst_queue_family_index(self.device.queue_family())
            .image(self.image)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            })
    }
}

impl std::fmt::Debug for Image {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Image")
            .field("size", &(self.width, self.height))
            .field("format", &self.format)
            .field("purpose", &self.purpose)
            .finish()
    }
}

impl Drop for Image {
    fn drop(&mut self) {
        let handle = self.device.handle();
        unsafe {
            // Nothing may still be reading it. A real frame loop will have
            // waited on its fence before dropping; this is the backstop.
            let _ = handle.device_wait_idle();
            handle.destroy_image_view(self.view, None);
            handle.destroy_image(self.image, None);
            handle.free_memory(self.memory, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{gbm_allocator, require_gpu, TestGpu};

    use smithay::backend::allocator::dmabuf::AsDmabuf;
    use smithay::backend::allocator::{Allocator, Fourcc};

    #[test]
    fn a_gbm_buffer_can_be_imported_as_a_sampleable_image() {
        let Some(TestGpu { device, node }) = require_gpu() else {
            return;
        };
        let mut allocator = match gbm_allocator(&node) {
            Some(allocator) => allocator,
            None => return,
        };

        // The modifiers the device actually advertises, rather than a guess:
        // an implicit-modifier buffer cannot be imported explicitly.
        let supported: Vec<_> = format::modifiers(device.physical(), Fourcc::Argb8888)
            .into_iter()
            .filter(|s| s.sampling && s.planes == 1)
            .map(|s| s.modifier)
            .collect();
        assert!(
            !supported.is_empty(),
            "{} advertises no sampleable ARGB8888 modifier",
            device.name()
        );

        let buffer = allocator
            .create_buffer(64, 64, Fourcc::Argb8888, &supported)
            .expect("gbm allocation");
        let dmabuf = buffer.export().expect("export");

        let image = Image::import(&device, &dmabuf, Purpose::Sample).expect("import");
        assert_eq!(image.width(), 64);
        assert_eq!(image.height(), 64);
        assert_eq!(image.format(), vk::Format::B8G8R8A8_UNORM);
        assert!(image.has_alpha());
        assert_ne!(image.view(), vk::ImageView::null());
    }

    #[test]
    fn a_render_target_can_be_imported() {
        let Some(TestGpu { device, node }) = require_gpu() else {
            return;
        };
        let mut allocator = match gbm_allocator(&node) {
            Some(allocator) => allocator,
            None => return,
        };

        let supported: Vec<_> = format::modifiers(device.physical(), Fourcc::Xrgb8888)
            .into_iter()
            .filter(|s| s.rendering && s.planes == 1)
            .map(|s| s.modifier)
            .collect();
        if supported.is_empty() {
            crate::test_support::skip("no renderable XRGB8888 modifier");
            return;
        }

        let buffer = allocator
            .create_buffer(128, 64, Fourcc::Xrgb8888, &supported)
            .expect("gbm allocation");
        let dmabuf = buffer.export().expect("export");

        let image = Image::import(&device, &dmabuf, Purpose::Render).expect("import");
        assert_eq!((image.width(), image.height()), (128, 64));
        // XRGB and ARGB are the same Vulkan format; only the alpha flag differs.
        assert_eq!(image.format(), vk::Format::B8G8R8A8_UNORM);
        assert!(!image.has_alpha());
    }

    #[test]
    fn the_first_acquire_takes_the_image_from_the_foreign_queue() {
        let Some(TestGpu { device, node }) = require_gpu() else {
            return;
        };
        let mut allocator = match gbm_allocator(&node) {
            Some(allocator) => allocator,
            None => return,
        };
        let supported: Vec<_> = format::modifiers(device.physical(), Fourcc::Argb8888)
            .into_iter()
            .filter(|s| s.sampling && s.planes == 1)
            .map(|s| s.modifier)
            .collect();
        if supported.is_empty() {
            crate::test_support::skip("no sampleable ARGB8888 modifier");
            return;
        }
        let dmabuf = allocator
            .create_buffer(32, 32, Fourcc::Argb8888, &supported)
            .expect("gbm allocation")
            .export()
            .expect("export");
        let image = Image::import(&device, &dmabuf, Purpose::Sample).expect("import");

        let first = image.acquire_barrier(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
        assert_eq!(
            first.src_queue_family_index,
            vk::QUEUE_FAMILY_FOREIGN_EXT,
            "the first acquire must claim the image from outside"
        );

        // Once claimed it is ours, and claiming it again would be a needless
        // ownership transfer the driver has to honour.
        let second = image.acquire_barrier(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
        assert_eq!(second.src_queue_family_index, device.queue_family());
    }

    #[test]
    fn an_unadvertised_modifier_is_refused_before_vulkan_sees_it() {
        use smithay::backend::allocator::dmabuf::DmabufFlags;
        use smithay::backend::allocator::Modifier;

        let Some(TestGpu { device, node }) = require_gpu() else {
            return;
        };
        let mut allocator = match gbm_allocator(&node) {
            Some(allocator) => allocator,
            None => return,
        };

        // A real buffer, relabelled with a modifier no driver implements.
        // Importing it would create an image whose memory layout does not
        // match its contents, which produces garbage rather than an error —
        // so this has to be caught before vkCreateImage.
        let real = allocator
            .create_buffer(32, 32, Fourcc::Argb8888, &[Modifier::Linear])
            .expect("gbm allocation")
            .export()
            .expect("export");

        let mut builder = Dmabuf::builder(
            (32, 32),
            Fourcc::Argb8888,
            Modifier::from(0x00ff_ffff_ffff_fffeu64),
            DmabufFlags::empty(),
        );
        let fd = real
            .handles()
            .next()
            .unwrap()
            .try_clone_to_owned()
            .expect("dup");
        builder.add_plane(fd, 0, real.strides().next().unwrap());
        let mislabelled = builder.build().expect("dmabuf builder");

        let error = Image::import(&device, &mislabelled, Purpose::Sample)
            .expect_err("an unadvertised modifier must not be imported");
        assert!(
            error.to_string().contains("does not support"),
            "unexpected error: {error}"
        );
    }
}
