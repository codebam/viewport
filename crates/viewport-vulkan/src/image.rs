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

/// What an image this renderer allocates is used for.
///
/// TRANSFER_SRC as well as TRANSFER_DST, because unlike an imported buffer
/// there is no modifier to negotiate — so it may as well be readable, which is
/// what a screenshot of an shm surface needs.
const ALLOCATED_USAGE: vk::ImageUsageFlags = vk::ImageUsageFlags::from_raw(
    vk::ImageUsageFlags::SAMPLED.as_raw()
        | vk::ImageUsageFlags::TRANSFER_DST.as_raw()
        | vk::ImageUsageFlags::TRANSFER_SRC.as_raw(),
);

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
            Self::Render => vk::ImageUsageFlags::COLOR_ATTACHMENT,
        }
    }
}

/// A Vulkan image backed by an imported DMA-BUF.
pub struct Image {
    device: Device,
    image: vk::Image,
    /// One entry per distinct allocation behind the image. A single-plane
    /// buffer, and a multi-planar one whose planes are offsets into one
    /// allocation, both have exactly one; only a disjoint import has more.
    memory: Vec<vk::DeviceMemory>,
    view: vk::ImageView,
    /// Set when this image is sampled through a YCbCr conversion, which the
    /// pipeline has to know because the conversion decides the sampler and
    /// the sampler is immutable in the descriptor set layout.
    ycbcr: Option<crate::device::YcbcrKey>,

    width: u32,
    height: u32,
    format: vk::Format,
    /// The DRM format it was imported as. Kept because `vk::Format` cannot
    /// distinguish XRGB from ARGB, and Smithay's `Texture` trait asks for the
    /// fourcc rather than the Vulkan format.
    fourcc: smithay::backend::allocator::Fourcc,
    has_alpha: bool,
    purpose: Purpose,
    usage: vk::ImageUsageFlags,

    /// Images arrive from outside this device's queue family, and the first
    /// barrier has to say so. True whenever somebody else owns it: at import,
    /// and again after every [`Image::release_barrier`] — a render target is
    /// handed back to the foreign queue at the end of each frame, so the next
    /// frame has to claim it again.
    ///
    /// An `AtomicBool` rather than a `Cell` so an `Image` is `Sync`. Smithay's
    /// `MemoryRenderBuffer` — which is how the cursor is drawn — keeps
    /// per-renderer textures in a shared map and requires it.
    foreign: std::sync::atomic::AtomicBool,
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

        let vk_format =
            format::to_vulkan(fourcc).ok_or_else(|| anyhow!("no Vulkan format for {fourcc:?}"))?;

        // Refuse a modifier the device did not advertise rather than letting
        // vkCreateImage fail with something less informative.
        let supported = format::modifiers(device.physical(), fourcc);
        let support = supported
            .iter()
            .find(|s| u64::from(s.modifier) == modifier)
            .ok_or_else(|| {
                anyhow!(
                    "{} does not support {fourcc:?} with modifier {modifier:#x}",
                    device.name()
                )
            })?;
        let yuv = format::is_yuv(fourcc);
        match purpose {
            Purpose::Sample if yuv && !support.ycbcr_sampling() => {
                // `sampling` on a multi-planar format only says the planes can
                // be read. Assembling them into colour is a separate feature,
                // and a device that cannot do it would otherwise produce a
                // greyscale video with no error anywhere.
                return Err(anyhow!(
                    "{fourcc:?} modifier {modifier:#x} cannot be sampled through a YCbCr conversion"
                ));
            }
            Purpose::Sample if !support.sampling => {
                return Err(anyhow!(
                    "{fourcc:?} modifier {modifier:#x} cannot be sampled"
                ))
            }
            Purpose::Render if !support.rendering => {
                return Err(anyhow!(
                    "{fourcc:?} modifier {modifier:#x} cannot be rendered into"
                ))
            }
            _ => {}
        }
        if yuv && purpose == Purpose::Render {
            return Err(anyhow!("{fourcc:?} cannot be a render target"));
        }
        if yuv && !device.has_ycbcr() {
            return Err(anyhow!("{} cannot sample multi-planar YUV", device.name()));
        }

        // Take the transfer usages the modifier actually advertises, and no
        // more. That is what makes read-back and blitting work where the
        // driver allows them without breaking imports where it does not.
        //
        // Never for YUV: a copy involving a multi-planar image names one plane
        // aspect at a time, and every caller here treats a transfer as covering
        // the whole image. Claiming the usage would make those calls compile,
        // pass, and copy a third of the picture.
        let mut usage = purpose.usage();
        if support.transfer_src && !yuv {
            usage |= vk::ImageUsageFlags::TRANSFER_SRC;
        }
        if support.transfer_dst && !yuv {
            usage |= vk::ImageUsageFlags::TRANSFER_DST;
        }

        let planes = buffer.num_planes();
        if planes as u32 != support.planes {
            return Err(anyhow!(
                "buffer has {planes} plane(s), modifier {modifier:#x} describes {}",
                support.planes
            ));
        }

        // How many allocations are actually behind the planes.
        //
        // A hardware decoder usually hands over one buffer with the planes at
        // different offsets, which binds as a single allocation. Some hand over
        // one fd per plane, which is a disjoint image and binds one allocation
        // per plane — a different code path, different create flags, and a
        // different bind call. The fds cannot be compared directly, because
        // two fds onto the same buffer are two different numbers; the
        // underlying object is what matters, and that is what the inode of the
        // DMA-BUF identifies.
        let allocations = distinct_allocations(buffer)?;
        // The number of *distinct* allocations, not of planes: two planes at
        // two offsets into one buffer both map to allocation 0, and that is
        // the common case.
        let distinct = allocations.iter().copied().max().unwrap_or(0) + 1;
        let disjoint = distinct > 1;
        if disjoint && !support.disjoint {
            return Err(anyhow!(
                "{fourcc:?} modifier {modifier:#x} spans {distinct} separate allocations, \
                 which this device cannot bind disjointly"
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

        let mut create_info = vk::ImageCreateInfo::default()
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
            .usage(usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            // UNDEFINED would discard the contents, which for an imported
            // client buffer is exactly the pixels we were given.
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut modifier_info)
            .push_next(&mut external);
        if disjoint {
            create_info = create_info.flags(vk::ImageCreateFlags::DISJOINT);
        }

        let handle = device.handle();
        let image = unsafe { handle.create_image(&create_info, None) }.context("vkCreateImage")?;

        // The conversion has to exist before the view, which names it, and it
        // is the same object the pipeline's immutable sampler will name.
        let ycbcr = if yuv {
            match ycbcr_key(device, fourcc, vk_format, height, support) {
                Ok(key) => Some(key),
                Err(e) => {
                    unsafe { handle.destroy_image(image, None) };
                    return Err(e);
                }
            }
        } else {
            None
        };

        // From here on every early return has to clean up what came before it.
        let result = (|| -> Result<(Vec<vk::DeviceMemory>, vk::ImageView)> {
            let memory = if disjoint {
                bind_disjoint(device, image, buffer, &allocations)?
            } else {
                vec![bind_whole(device, image, buffer)?]
            };

            let mut conversion_info = ycbcr
                .map(|key| device.ycbcr_conversion(key))
                .transpose()
                .inspect_err(|_| {
                    for memory in &memory {
                        unsafe { handle.free_memory(*memory, None) };
                    }
                })?
                .map(|conversion| vk::SamplerYcbcrConversionInfo::default().conversion(conversion));

            let mut view_info = vk::ImageViewCreateInfo::default()
                .image(image)
                .view_type(vk::ImageViewType::TYPE_2D)
                .format(vk_format)
                .subresource_range(vk::ImageSubresourceRange {
                    // COLOR, not the per-plane aspects: the view is of the
                    // whole image, and the conversion is what turns the planes
                    // underneath it into one sample.
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: 0,
                    layer_count: 1,
                });
            if let Some(info) = conversion_info.as_mut() {
                view_info = view_info.push_next(info);
            }

            let view = match unsafe { handle.create_image_view(&view_info, None) } {
                Ok(view) => view,
                Err(e) => {
                    for memory in &memory {
                        unsafe { handle.free_memory(*memory, None) };
                    }
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
            ycbcr,
            width,
            height,
            format: vk_format,
            fourcc,
            has_alpha: format::has_alpha(fourcc),
            purpose,
            usage,
            foreign: std::sync::atomic::AtomicBool::new(true),
        })
    }

    /// Allocate an image this renderer owns outright.
    ///
    /// Unlike [`Image::import`] there is no DMA-BUF and no modifier: the
    /// driver picks an optimal tiling, because nothing outside this device
    /// will ever look at the memory. This is what shm client buffers are
    /// copied into.
    pub fn allocate(
        device: &Device,
        width: u32,
        height: u32,
        fourcc: smithay::backend::allocator::Fourcc,
    ) -> Result<Self> {
        let vk_format =
            format::to_vulkan(fourcc).ok_or_else(|| anyhow!("no Vulkan format for {fourcc:?}"))?;
        let handle = device.handle();

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
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(ALLOCATED_USAGE)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);

        let image = unsafe { handle.create_image(&create_info, None) }.context("vkCreateImage")?;

        let result = (|| -> Result<(vk::DeviceMemory, vk::ImageView)> {
            let requirements = unsafe { handle.get_image_memory_requirements(image) };
            let memory_type = device
                .memory_type_with(requirements.memory_type_bits, |flags| {
                    flags.contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
                })
                .ok_or_else(|| anyhow!("no device-local memory type"))?;

            let allocate = vk::MemoryAllocateInfo::default()
                .allocation_size(requirements.size)
                .memory_type_index(memory_type);
            let memory =
                unsafe { handle.allocate_memory(&allocate, None) }.context("vkAllocateMemory")?;

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
            memory: vec![memory],
            view,
            // Allocated images are what shm buffers are copied into, and that
            // copy targets ordinary colour.
            ycbcr: None,
            width,
            height,
            format: vk_format,
            fourcc,
            has_alpha: format::has_alpha(fourcc),
            purpose: Purpose::Sample,
            usage: ALLOCATED_USAGE,
            // Ours from the moment it is created: there is no other queue
            // family that could have owned it.
            foreign: std::sync::atomic::AtomicBool::new(false),
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

    /// The conversion this image is sampled through, if it is YUV.
    ///
    /// The pipeline needs it: a conversion makes the sampler immutable in the
    /// descriptor set layout, so an image sampled through one cannot use the
    /// ordinary texture pipeline.
    pub fn ycbcr(&self) -> Option<crate::device::YcbcrKey> {
        self.ycbcr
    }

    pub fn purpose(&self) -> Purpose {
        self.purpose
    }

    /// Whether this image can be the source of a copy — that is, whether it
    /// can be read back into memory at all.
    pub fn is_readable(&self) -> bool {
        self.usage.contains(vk::ImageUsageFlags::TRANSFER_SRC)
    }

    /// Whether this image can be the destination of a copy or a blit.
    pub fn is_writable(&self) -> bool {
        self.usage.contains(vk::ImageUsageFlags::TRANSFER_DST)
    }

    /// A barrier handing the image back to whoever will consume it next.
    ///
    /// After rendering, the image goes to KMS, to another API, or to a CPU
    /// mapping — none of which are this queue family. Releasing it to
    /// `VK_QUEUE_FAMILY_FOREIGN_EXT` in `GENERAL` layout is what makes the
    /// contents defined for all of them.
    ///
    /// And it is foreign again afterwards. A target is bound once and reused
    /// for every frame it is the output's buffer, so without this the release
    /// happens each frame while the matching acquire happens only on the
    /// first: from the second frame on, rendering starts on an image this
    /// queue family does not own — which is undefined, and undefined in the
    /// way that works on the driver it was written against.
    pub fn release_barrier(&self, from: vk::ImageLayout) -> vk::ImageMemoryBarrier<'static> {
        self.foreign
            .store(true, std::sync::atomic::Ordering::Relaxed);
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
    ///
    /// `from` is `GENERAL`, not `UNDEFINED`, and the difference matters:
    /// `UNDEFINED` tells the driver the contents may be thrown away, which for
    /// a client buffer means the pixels the client just painted. `GENERAL` is
    /// the layout an image written by a non-Vulkan API is conventionally in,
    /// and it preserves them. Passing `UNDEFINED` is only correct for a target
    /// about to be completely overwritten.
    pub fn acquire_barrier(&self, layout: vk::ImageLayout) -> vk::ImageMemoryBarrier<'static> {
        self.acquire_barrier_from(vk::ImageLayout::GENERAL, layout)
    }

    /// [`Image::acquire_barrier`] with an explicit source layout.
    pub fn acquire_barrier_from(
        &self,
        from: vk::ImageLayout,
        to: vk::ImageLayout,
    ) -> vk::ImageMemoryBarrier<'static> {
        let src_family = if self
            .foreign
            .swap(false, std::sync::atomic::Ordering::Relaxed)
        {
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
            .old_layout(from)
            .new_layout(to)
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

    /// A plain layout transition on an image this renderer already owns.
    pub fn transition(
        &self,
        from: vk::ImageLayout,
        to: vk::ImageLayout,
        src_access: vk::AccessFlags,
        dst_access: vk::AccessFlags,
    ) -> vk::ImageMemoryBarrier<'static> {
        vk::ImageMemoryBarrier::default()
            .src_access_mask(src_access)
            .dst_access_mask(dst_access)
            .old_layout(from)
            .new_layout(to)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
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

/// One entry per plane: which distinct allocation it lives in.
///
/// `[0, 0]` is a two-plane buffer packed into one DMA-BUF, which is what a
/// hardware decoder normally produces. `[0, 1]` is one DMA-BUF per plane, which
/// has to be bound disjointly.
fn distinct_allocations(buffer: &Dmabuf) -> Result<Vec<usize>> {
    // Two fds onto the same buffer are two different numbers, so the fds
    // cannot be compared. The DMA-BUF itself is a file on an anonymous
    // filesystem, and its inode identifies it — which is how everything else
    // that has to answer this question answers it.
    let mut identities: Vec<(u64, u64)> = Vec::new();
    let mut which = Vec::new();
    for fd in buffer.handles() {
        // SAFETY: `fd` is a valid borrowed descriptor for the length of the
        // loop body, and `stat` is fully written on success.
        let identity = unsafe {
            let mut stat: libc::stat = std::mem::zeroed();
            if libc::fstat(fd.as_raw_fd(), &mut stat) != 0 {
                return Err(anyhow::Error::from(std::io::Error::last_os_error())
                    .context("fstat on a dmabuf plane"));
            }
            (stat.st_dev as u64, stat.st_ino as u64)
        };
        which.push(match identities.iter().position(|seen| *seen == identity) {
            Some(index) => index,
            None => {
                identities.push(identity);
                identities.len() - 1
            }
        });
    }
    if which.is_empty() {
        return Err(anyhow!("dmabuf has no planes"));
    }
    Ok(which)
}

/// Import a plane's fd and allocate the memory behind it.
///
/// `requirements` differs between the whole-image and per-plane cases, which is
/// the only reason this is a parameter rather than read here.
fn import_plane(
    device: &Device,
    image: vk::Image,
    fd: std::os::fd::BorrowedFd<'_>,
    requirements: vk::MemoryRequirements,
) -> Result<vk::DeviceMemory> {
    // The driver says which memory types this specific fd can back.
    let mut fd_properties = vk::MemoryFdPropertiesKHR::default();
    unsafe {
        device.external_memory_fd().get_memory_fd_properties(
            HANDLE_TYPE,
            fd.as_raw_fd(),
            &mut fd_properties,
        )
    }
    .context("vkGetMemoryFdPropertiesKHR")?;

    let memory_type = device
        .memory_type(
            requirements.memory_type_bits,
            fd_properties.memory_type_bits,
        )
        .ok_or_else(|| anyhow!("no memory type can back this dmabuf"))?;

    // vkAllocateMemory consumes the fd on success, so hand it a copy.
    let owned: OwnedFd = fd.try_clone_to_owned().context("dup dmabuf fd")?;

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
        unsafe { device.handle().allocate_memory(&allocate, None) }.context("vkAllocateMemory")?;
    // Ownership passed to Vulkan; dropping it here would close a fd the driver
    // still holds.
    std::mem::forget(owned);
    Ok(memory)
}

/// Bind an image whose planes all live in one allocation.
fn bind_whole(device: &Device, image: vk::Image, buffer: &Dmabuf) -> Result<vk::DeviceMemory> {
    let handle = device.handle();
    let requirements = unsafe { handle.get_image_memory_requirements(image) };
    let fd = buffer
        .handles()
        .next()
        .ok_or_else(|| anyhow!("dmabuf has no planes"))?;
    let memory = import_plane(device, image, fd, requirements)?;
    if let Err(e) = unsafe { handle.bind_image_memory(image, memory, 0) } {
        unsafe { handle.free_memory(memory, None) };
        return Err(anyhow::Error::from(e).context("vkBindImageMemory"));
    }
    Ok(memory)
}

/// Bind an image whose planes live in separate allocations.
///
/// Every plane is asked about separately — its memory requirements are its
/// own — and all of them are bound in one call, which is what
/// `vkBindImageMemory2` is for. The aspect is `MEMORY_PLANE_i`, not `PLANE_i`:
/// with a DRM modifier the memory planes are what the modifier lays out, and
/// for a compressed format there are more of them than there are colour planes.
fn bind_disjoint(
    device: &Device,
    image: vk::Image,
    buffer: &Dmabuf,
    allocations: &[usize],
) -> Result<Vec<vk::DeviceMemory>> {
    const ASPECTS: [vk::ImageAspectFlags; 4] = [
        vk::ImageAspectFlags::MEMORY_PLANE_0_EXT,
        vk::ImageAspectFlags::MEMORY_PLANE_1_EXT,
        vk::ImageAspectFlags::MEMORY_PLANE_2_EXT,
        vk::ImageAspectFlags::MEMORY_PLANE_3_EXT,
    ];

    let handle = device.handle();
    let fds: Vec<_> = buffer.handles().collect();
    let count = allocations.len();
    anyhow::ensure!(
        count <= ASPECTS.len(),
        "a dmabuf with {count} planes is more than Vulkan describes"
    );

    let mut memories: Vec<vk::DeviceMemory> = Vec::with_capacity(count);
    let free_all = |memories: &[vk::DeviceMemory]| {
        for memory in memories {
            unsafe { handle.free_memory(*memory, None) };
        }
    };

    for (plane, aspect) in ASPECTS.iter().enumerate().take(count) {
        let mut plane_info = vk::ImagePlaneMemoryRequirementsInfo::default().plane_aspect(*aspect);
        let info = vk::ImageMemoryRequirementsInfo2::default()
            .image(image)
            .push_next(&mut plane_info);
        let mut requirements = vk::MemoryRequirements2::default();
        unsafe { handle.get_image_memory_requirements2(&info, &mut requirements) };

        match import_plane(device, image, fds[plane], requirements.memory_requirements) {
            Ok(memory) => memories.push(memory),
            Err(e) => {
                free_all(&memories);
                return Err(e);
            }
        }
    }

    // Built in two passes because each bind holds a pointer into its own plane
    // info, and those have to outlive the call.
    let mut plane_infos: Vec<vk::BindImagePlaneMemoryInfo> = ASPECTS
        .iter()
        .take(count)
        .map(|aspect| vk::BindImagePlaneMemoryInfo::default().plane_aspect(*aspect))
        .collect();
    let binds: Vec<vk::BindImageMemoryInfo> = plane_infos
        .iter_mut()
        .zip(&memories)
        .map(|(plane_info, memory)| {
            vk::BindImageMemoryInfo::default()
                .image(image)
                .memory(*memory)
                .memory_offset(0)
                .push_next(plane_info)
        })
        .collect();

    if let Err(e) = unsafe { handle.bind_image_memory2(&binds) } {
        free_all(&memories);
        return Err(anyhow::Error::from(e).context("vkBindImageMemory2"));
    }
    Ok(memories)
}

/// The conversion a YUV buffer of this shape needs.
///
/// Neither the matrix nor the range is carried by a DMA-BUF, so both are
/// inferred. Height decides the matrix, which is the same rule every video
/// stack uses: anything of standard-definition height predates BT.709 and was
/// almost certainly encoded with BT.601, and everything taller was not. The
/// range is taken as narrow because that is what broadcast and every hardware
/// decoder default to; a full-range buffer read as narrow comes out slightly
/// washed out rather than wrong.
///
/// The siting is not a guess: it is whichever of the two the device says it can
/// reconstruct, preferring the one the MPEG family actually uses.
fn ycbcr_key(
    device: &Device,
    fourcc: smithay::backend::allocator::Fourcc,
    vk_format: vk::Format,
    height: u32,
    support: &format::ModifierSupport,
) -> Result<crate::device::YcbcrKey> {
    use smithay::backend::allocator::Fourcc;

    // 576 is PAL's active height, the tallest standard-definition format.
    let model = if height <= 576 {
        vk::SamplerYcbcrModelConversion::YCBCR_601
    } else {
        vk::SamplerYcbcrModelConversion::YCBCR_709
    };

    // Horizontally the MPEG family sites chroma on the left-hand luma sample.
    let x_offset = if support.cosited_chroma {
        vk::ChromaLocation::COSITED_EVEN
    } else {
        vk::ChromaLocation::MIDPOINT
    };
    // Vertically, only a 4:2:0 format has a choice to make: without vertical
    // subsampling the chroma sample sits on the luma row by definition, and
    // saying anything else is invalid.
    let subsampled_vertically = matches!(
        fourcc,
        Fourcc::Nv12 | Fourcc::Nv21 | Fourcc::Yuv420 | Fourcc::Yvu420 | Fourcc::P010 | Fourcc::P016
    );
    let y_offset = if subsampled_vertically && support.midpoint_chroma {
        vk::ChromaLocation::MIDPOINT
    } else {
        vk::ChromaLocation::COSITED_EVEN
    };

    // Chroma is filtered linearly only where the device says it can be. Where
    // it cannot, luma has to drop to nearest with it — Vulkan requires the two
    // filters to match, and a mismatch is invalid usage rather than a
    // best-effort.
    let filter = if support.linear_chroma {
        vk::Filter::LINEAR
    } else {
        vk::Filter::NEAREST
    };

    let key = crate::device::YcbcrKey {
        format: vk_format,
        model,
        range: vk::SamplerYcbcrRange::ITU_NARROW,
        x_offset,
        y_offset,
        filter,
        swapped_chroma: matches!(fourcc, Fourcc::Nv21 | Fourcc::Yvu420),
    };
    // Created here rather than at first draw so a device that cannot make it
    // fails the import, where the error names the buffer.
    device.ycbcr_conversion(key)?;
    Ok(key)
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
            for memory in &self.memory {
                handle.free_memory(*memory, None);
            }
            // The conversion is not freed here: it is the device's, shared
            // with every other image of this format and with the sampler the
            // pipeline was built around.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{gbm_allocator, linear_nv12, require_gpu, TestGpu};

    use smithay::backend::allocator::dmabuf::AsDmabuf;
    use smithay::backend::allocator::{Allocator, Fourcc, Modifier};

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

    /// A render target is bound once and drawn into every frame, and every
    /// frame ends by releasing it to the foreign queue. So the frame after
    /// that has to take it back, exactly as the first one did.
    #[test]
    fn a_released_image_is_foreign_again() {
        let Some(TestGpu { device, node }) = require_gpu() else {
            return;
        };
        let mut allocator = match gbm_allocator(&node) {
            Some(allocator) => allocator,
            None => return,
        };
        let supported: Vec<_> = format::modifiers(device.physical(), Fourcc::Argb8888)
            .into_iter()
            .filter(|s| s.rendering && s.planes == 1)
            .map(|s| s.modifier)
            .collect();
        if supported.is_empty() {
            crate::test_support::skip("no renderable ARGB8888 modifier");
            return;
        }
        let dmabuf = allocator
            .create_buffer(32, 32, Fourcc::Argb8888, &supported)
            .expect("gbm allocation")
            .export()
            .expect("export");
        let image = Image::import(&device, &dmabuf, Purpose::Render).expect("import");

        // Frame one, whole: claim it, draw, hand it back.
        let _ = image.acquire_barrier(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL);
        let release = image.release_barrier(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL);
        assert_eq!(release.dst_queue_family_index, vk::QUEUE_FAMILY_FOREIGN_EXT);

        // Frame two starts where frame one left it.
        let again = image.acquire_barrier(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL);
        assert_eq!(
            again.src_queue_family_index,
            vk::QUEUE_FAMILY_FOREIGN_EXT,
            "a target released to the foreign queue has to be claimed back"
        );
    }

    /// Whether this device can sample NV12 at all, and would let a test that
    /// ran mean anything.
    fn nv12_is_sampleable(device: &Device) -> bool {
        if !device.has_ycbcr() {
            crate::test_support::skip("the device cannot sample YUV");
            return false;
        }
        let linear = format::modifiers(device.physical(), Fourcc::Nv12)
            .into_iter()
            .any(|s| s.modifier == Modifier::Linear && s.ycbcr_sampling());
        if !linear {
            crate::test_support::skip("no YCbCr-sampleable linear NV12");
        }
        linear
    }

    #[test]
    fn an_nv12_buffer_imports_as_one_image_sampled_through_a_conversion() {
        // The whole point of the multi-planar path: a frame shaped like what a
        // hardware decoder produces, imported without a CPU convert. Before
        // this it was refused at `to_vulkan`, and every video player fell back
        // to converting each frame on the CPU.
        let Some(TestGpu { device, node }) = require_gpu() else {
            return;
        };
        let mut allocator = match gbm_allocator(&node) {
            Some(allocator) => allocator,
            None => return,
        };
        if !nv12_is_sampleable(&device) {
            return;
        }
        let Some(dmabuf) = linear_nv12(&mut allocator, 256, 64) else {
            return;
        };
        assert_eq!(dmabuf.num_planes(), 2, "NV12 is two planes");

        let image = Image::import(&device, &dmabuf, Purpose::Sample).expect("import");
        assert_eq!((image.width(), image.height()), (256, 64));
        assert_eq!(image.format(), vk::Format::G8_B8R8_2PLANE_420_UNORM);
        // No alpha, so it composites opaque rather than blending against
        // undefined bytes.
        assert!(!image.has_alpha());
        // The conversion is what makes it sample as colour, and the pipeline
        // reads this to pick the layout with the matching immutable sampler.
        let key = image
            .ycbcr()
            .expect("an NV12 image must carry a conversion");
        assert_eq!(key.format, vk::Format::G8_B8R8_2PLANE_420_UNORM);
        assert!(!key.swapped_chroma);
        assert_eq!(key.range, vk::SamplerYcbcrRange::ITU_NARROW);
        assert_ne!(image.view(), vk::ImageView::null());
    }

    #[test]
    fn standard_definition_gets_the_matrix_it_was_encoded_with() {
        // 601 below PAL's active height, 709 above it. Using one matrix for
        // both is the difference between correct colour and a green cast on
        // everything old, and neither the buffer nor the protocol says which.
        let Some(TestGpu { device, node }) = require_gpu() else {
            return;
        };
        let mut allocator = match gbm_allocator(&node) {
            Some(allocator) => allocator,
            None => return,
        };
        if !nv12_is_sampleable(&device) {
            return;
        }

        let Some(sd) = linear_nv12(&mut allocator, 256, 480) else {
            return;
        };
        let sd = Image::import(&device, &sd, Purpose::Sample).expect("import");
        assert_eq!(
            sd.ycbcr().expect("a conversion").model,
            vk::SamplerYcbcrModelConversion::YCBCR_601
        );

        let Some(hd) = linear_nv12(&mut allocator, 256, 720) else {
            return;
        };
        let hd = Image::import(&device, &hd, Purpose::Sample).expect("import");
        assert_eq!(
            hd.ycbcr().expect("a conversion").model,
            vk::SamplerYcbcrModelConversion::YCBCR_709
        );
    }

    #[test]
    fn a_video_frame_is_never_offered_as_a_render_target() {
        // Nothing composites *into* YUV, and an image the rest of the renderer
        // treats as an RGB colour attachment is one it will happily clear and
        // blend in a colour space that does not exist.
        let Some(TestGpu { device, node }) = require_gpu() else {
            return;
        };
        let mut allocator = match gbm_allocator(&node) {
            Some(allocator) => allocator,
            None => return,
        };
        if !nv12_is_sampleable(&device) {
            return;
        }
        let Some(dmabuf) = linear_nv12(&mut allocator, 256, 64) else {
            return;
        };

        let error = Image::import(&device, &dmabuf, Purpose::Render)
            .expect_err("YUV must not be a render target");
        let message = error.to_string();
        assert!(
            message.contains("render target") || message.contains("cannot be rendered into"),
            "unexpected error: {message}"
        );
    }

    #[test]
    fn an_imported_video_frame_cannot_be_copied_out_of() {
        // A copy on a multi-planar image names one plane aspect at a time, and
        // every transfer in this renderer covers the whole image. Claiming the
        // usage would make a screenshot of a video succeed and return a third
        // of the picture.
        let Some(TestGpu { device, node }) = require_gpu() else {
            return;
        };
        let mut allocator = match gbm_allocator(&node) {
            Some(allocator) => allocator,
            None => return,
        };
        if !nv12_is_sampleable(&device) {
            return;
        }
        let Some(dmabuf) = linear_nv12(&mut allocator, 256, 64) else {
            return;
        };

        let image = Image::import(&device, &dmabuf, Purpose::Sample).expect("import");
        assert!(!image.is_readable());
        assert!(!image.is_writable());
    }

    #[test]
    fn planes_in_one_allocation_are_not_mistaken_for_a_disjoint_image() {
        // The two planes are two fds onto the same buffer, which are two
        // different numbers. Comparing the descriptors would call this disjoint
        // and bind each plane its own allocation — on a device that mostly does
        // not support disjoint at all, so the import would simply fail.
        let Some(TestGpu { device, node }) = require_gpu() else {
            return;
        };
        let mut allocator = match gbm_allocator(&node) {
            Some(allocator) => allocator,
            None => return,
        };
        if !nv12_is_sampleable(&device) {
            return;
        }
        let Some(dmabuf) = linear_nv12(&mut allocator, 256, 64) else {
            return;
        };

        let fds: Vec<_> = dmabuf.handles().map(|fd| fd.as_raw_fd()).collect();
        assert_eq!(fds.len(), 2);
        assert_ne!(fds[0], fds[1], "the planes must be separate descriptors");
        assert_eq!(
            distinct_allocations(&dmabuf).expect("stat"),
            vec![0, 0],
            "both planes live in one allocation"
        );
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
