// SPDX-License-Identifier: MIT
//
// Picking a GPU and opening a logical device on it.
//
// The device has to be the *same* GPU the buffers come from. A compositor
// receives DMA-BUFs allocated by its clients and by the web engine, and
// importing one into a device on another card either fails or silently copies
// over PCIe. So the physical device is selected by matching its DRM render
// node against the node the rest of the compositor is using, rather than by
// taking the first discrete GPU and hoping.

use std::ffi::CStr;
use std::sync::Arc;

use anyhow::{anyhow, Context as _, Result};
use ash::vk;
use smithay::backend::drm::DrmNode;
use smithay::backend::vulkan::{version::Version, Instance, PhysicalDevice};

/// Extensions the renderer cannot work without.
///
/// All of these exist to move images between APIs and processes without a
/// copy. `image_drm_format_modifier` is the one that makes a Vulkan image
/// describable in the same terms as a DMA-BUF — without it, a tiled buffer
/// from another API is uninterpretable.
pub const REQUIRED_EXTENSIONS: &[&CStr] = &[
    vk::KHR_EXTERNAL_MEMORY_FD_NAME,
    vk::EXT_EXTERNAL_MEMORY_DMA_BUF_NAME,
    vk::EXT_IMAGE_DRM_FORMAT_MODIFIER_NAME,
    vk::KHR_IMAGE_FORMAT_LIST_NAME,
    vk::EXT_QUEUE_FAMILY_FOREIGN_NAME,
    vk::KHR_EXTERNAL_SEMAPHORE_FD_NAME,
    // Core in 1.3. Rendering without VkRenderPass and VkFramebuffer objects,
    // which for a compositor is pure subtraction: every frame targets a
    // different imported image, so a cache of render passes keyed by format
    // would be rebuilt constantly and buy nothing.
    vk::KHR_DYNAMIC_RENDERING_NAME,
    // Descriptors pushed straight into the command buffer. A compositor binds
    // a different texture for every surface every frame, so the alternative is
    // a descriptor pool that has to be sized, allocated from and recycled —
    // all of it bookkeeping this avoids entirely.
    vk::KHR_PUSH_DESCRIPTOR_NAME,
];

/// Wanted, but the renderer degrades rather than fails without them.
pub const OPTIONAL_EXTENSIONS: &[&CStr] = &[
    // Lets a fence come back out as a sync_file fd, which is what a
    // drm_syncobj timeline takes. Without it the compositor waits on the CPU.
    vk::KHR_TIMELINE_SEMAPHORE_NAME,
];

/// The minimum Vulkan version. 1.2 for timeline semaphores and descriptor
/// indexing, both core there rather than extensions.
pub const MINIMUM_VERSION: Version = Version::VERSION_1_2;

/// An open Vulkan device on a specific GPU.
///
/// Cheap to clone: every image, framebuffer and command buffer holds one so it
/// can clean itself up, and the underlying device is destroyed when the last
/// of them goes.
#[derive(Clone)]
pub struct Device(Arc<Inner>);

struct Inner {
    physical: PhysicalDevice,
    device: ash::Device,
    queue: vk::Queue,
    queue_family: u32,
    has_timeline_semaphores: bool,

    /// Loaded once. Each of these is a table of function pointers fetched with
    /// vkGetDeviceProcAddr, so building one per import would be pure overhead.
    external_memory_fd: ash::khr::external_memory_fd::Device,
    dynamic_rendering: ash::khr::dynamic_rendering::Device,
    push_descriptor: ash::khr::push_descriptor::Device,
    external_semaphore_fd: ash::khr::external_semaphore_fd::Device,
}

impl Device {
    /// Open the GPU exposing `node`.
    ///
    /// `node` is normally the render node the compositor already has open —
    /// `/dev/dri/renderD128` and friends.
    pub fn for_node(instance: &Instance, node: &DrmNode) -> Result<Self> {
        let devices: Vec<PhysicalDevice> = PhysicalDevice::enumerate(instance)
            .context("vkEnumeratePhysicalDevices")?
            .collect();
        if devices.is_empty() {
            return Err(anyhow!(
                "no Vulkan device at all. The renderer is Vulkan and falls back \n\
                 to nothing, so a driver has to be installed: mesa's vulkan-radeon \n\
                 or vulkan-intel on real hardware, vulkan-virtio in a virtual \n\
                 machine with 3D acceleration, or vulkan-swrast to render in \n\
                 software."
            ));
        }

        let matching = devices.iter().find(|device| {
            // A device may expose a primary node, a render node, or both.
            // Matching either is what makes this work whether the caller
            // opened /dev/dri/card1 or /dev/dri/renderD128.
            matches!(device.render_node(), Ok(Some(n)) if n == *node)
                || matches!(device.primary_node(), Ok(Some(n)) if n == *node)
        });

        if let Some(physical) = matching {
            return Self::open(physical.clone());
        }

        // Nothing owns the display's node. A software renderer is the usual
        // reason: lavapipe exposes no DRM node at all, so it can never match,
        // and a virtual machine without 3D acceleration has nothing else. It
        // can still draw — it imports the compositor's buffers through
        // VK_EXT_external_memory_dma_buf like any other device — so refusing
        // here means refusing to start on every such machine, which is what
        // it did.
        //
        // Said loudly, because every frame is then drawn on the CPU and copied,
        // and somebody wondering why their desktop is slow deserves the reason
        // in the log rather than a guess.
        let physical = devices
            .into_iter()
            .next()
            .expect("checked non-empty above");
        tracing::warn!(
            "no Vulkan device exposes {node:?}; falling back to {}. \
             Every frame will be drawn without the display's own GPU, which is \
             correct but slow — install the driver for this device (vulkan-virtio \
             for a virtual machine with 3D acceleration) to avoid it.",
            physical.name()
        );
        Self::open(physical)
    }

    /// Open a specific physical device.
    pub fn open(physical: PhysicalDevice) -> Result<Self> {
        if physical.api_version() < MINIMUM_VERSION {
            return Err(anyhow!(
                "{} reports Vulkan {:?}, need at least {MINIMUM_VERSION:?}",
                physical.name(),
                physical.api_version()
            ));
        }

        let missing: Vec<&CStr> = REQUIRED_EXTENSIONS
            .iter()
            .copied()
            .filter(|extension| !physical.has_device_extension(extension))
            .collect();
        if !missing.is_empty() {
            return Err(anyhow!(
                "{} is missing {}",
                physical.name(),
                missing
                    .iter()
                    .map(|e| e.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }

        let instance = physical.instance().handle();
        let handle = physical.handle();

        // One graphics queue is enough: this renderer composites, it does not
        // run async compute, and a second queue would only add ownership
        // transfers between families.
        let queue_family = unsafe { instance.get_physical_device_queue_family_properties(handle) }
            .iter()
            .position(|family| family.queue_flags.contains(vk::QueueFlags::GRAPHICS))
            .ok_or_else(|| anyhow!("{} has no graphics queue", physical.name()))?
            as u32;

        let enabled: Vec<&CStr> = REQUIRED_EXTENSIONS
            .iter()
            .copied()
            .chain(
                OPTIONAL_EXTENSIONS
                    .iter()
                    .copied()
                    .filter(|e| physical.has_device_extension(e)),
            )
            .collect();
        let enabled_ptrs: Vec<*const std::os::raw::c_char> =
            enabled.iter().map(|e| e.as_ptr()).collect();

        let priorities = [1.0f32];
        let queue_info = vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family)
            .queue_priorities(&priorities);

        // Core in 1.2 and 1.3 respectively, but both still have to be asked for.
        let mut timeline =
            vk::PhysicalDeviceTimelineSemaphoreFeatures::default().timeline_semaphore(true);
        let mut dynamic_rendering =
            vk::PhysicalDeviceDynamicRenderingFeatures::default().dynamic_rendering(true);

        let queue_infos = [queue_info];
        let create_info = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queue_infos)
            .enabled_extension_names(&enabled_ptrs)
            .push_next(&mut timeline)
            .push_next(&mut dynamic_rendering);

        let device = unsafe { instance.create_device(handle, &create_info, None) }
            .context("vkCreateDevice")?;
        let queue = unsafe { device.get_device_queue(queue_family, 0) };
        let external_memory_fd = ash::khr::external_memory_fd::Device::new(instance, &device);
        let dynamic_rendering = ash::khr::dynamic_rendering::Device::new(instance, &device);
        let push_descriptor = ash::khr::push_descriptor::Device::new(instance, &device);
        let external_semaphore_fd =
            ash::khr::external_semaphore_fd::Device::new(instance, &device);

        let has_timeline_semaphores = enabled.contains(&vk::KHR_TIMELINE_SEMAPHORE_NAME)
            || physical.api_version() >= Version::VERSION_1_2;

        tracing::info!(
            "vulkan device {} ({:?}), queue family {queue_family}",
            physical.name(),
            physical.ty()
        );

        Ok(Self(Arc::new(Inner {
            physical,
            device,
            queue,
            queue_family,
            has_timeline_semaphores,
            external_memory_fd,
            dynamic_rendering,
            push_descriptor,
            external_semaphore_fd,
        })))
    }

    pub fn external_memory_fd(&self) -> &ash::khr::external_memory_fd::Device {
        &self.0.external_memory_fd
    }

    pub fn dynamic_rendering(&self) -> &ash::khr::dynamic_rendering::Device {
        &self.0.dynamic_rendering
    }

    pub fn push_descriptor(&self) -> &ash::khr::push_descriptor::Device {
        &self.0.push_descriptor
    }

    pub fn external_semaphore_fd(&self) -> &ash::khr::external_semaphore_fd::Device {
        &self.0.external_semaphore_fd
    }

    /// The index of a memory type satisfying `requirements` and allowed by
    /// `allowed`, preferring device-local memory.
    ///
    /// `allowed` comes from `vkGetMemoryFdPropertiesKHR` when importing: the
    /// driver decides which memory types an imported fd is compatible with,
    /// and intersecting that with the image's own requirements is what stops
    /// an import binding memory the GPU cannot actually read.
    /// The index of a memory type satisfying `requirements` whose property
    /// flags pass `wanted`.
    ///
    /// Used for memory this renderer allocates itself, where the constraint is
    /// a property — host-visible, device-local — rather than compatibility
    /// with an imported fd.
    pub fn memory_type_with<F>(&self, requirements: u32, wanted: F) -> Option<u32>
    where
        F: Fn(vk::MemoryPropertyFlags) -> bool,
    {
        let instance = self.0.physical.instance().handle();
        let properties =
            unsafe { instance.get_physical_device_memory_properties(self.0.physical.handle()) };

        (0..properties.memory_type_count).find(|index| {
            requirements & (1 << index) != 0
                && wanted(properties.memory_types[*index as usize].property_flags)
        })
    }

    pub fn memory_type(&self, requirements: u32, allowed: u32) -> Option<u32> {
        let instance = self.0.physical.instance().handle();
        let properties =
            unsafe { instance.get_physical_device_memory_properties(self.0.physical.handle()) };
        let candidates = requirements & allowed;

        (0..properties.memory_type_count)
            .filter(|index| candidates & (1 << index) != 0)
            .max_by_key(|index| {
                properties.memory_types[*index as usize]
                    .property_flags
                    .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
            })
    }

    pub fn handle(&self) -> &ash::Device {
        &self.0.device
    }

    pub fn physical(&self) -> &PhysicalDevice {
        &self.0.physical
    }

    pub fn queue(&self) -> vk::Queue {
        self.0.queue
    }

    pub fn queue_family(&self) -> u32 {
        self.0.queue_family
    }

    pub fn supports_timeline_semaphores(&self) -> bool {
        self.0.has_timeline_semaphores
    }

    pub fn name(&self) -> &str {
        self.0.physical.name()
    }
}

impl std::fmt::Debug for Device {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Device")
            .field("name", &self.0.physical.name())
            .field("queue_family", &self.0.queue_family)
            .field("timeline_semaphores", &self.0.has_timeline_semaphores)
            .finish()
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        unsafe {
            // Everything submitted has to have finished before the device goes
            // away; the alternative is a use-after-free inside the driver.
            let _ = self.device.device_wait_idle();
            self.device.destroy_device(None);
        }
    }
}
