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
pub struct Device {
    physical: PhysicalDevice,
    device: Arc<ash::Device>,
    queue: vk::Queue,
    queue_family: u32,
    has_timeline_semaphores: bool,
}

impl Device {
    /// Open the GPU exposing `node`.
    ///
    /// `node` is normally the render node the compositor already has open —
    /// `/dev/dri/renderD128` and friends.
    pub fn for_node(instance: &Instance, node: &DrmNode) -> Result<Self> {
        let physical = PhysicalDevice::enumerate(instance)
            .context("vkEnumeratePhysicalDevices")?
            .find(|device| {
                // A device may expose a primary node, a render node, or both.
                // Matching either is what makes this work whether the caller
                // opened /dev/dri/card1 or /dev/dri/renderD128.
                matches!(device.render_node(), Ok(Some(n)) if n == *node)
                    || matches!(device.primary_node(), Ok(Some(n)) if n == *node)
            })
            .ok_or_else(|| anyhow!("no Vulkan device exposes {node:?}"))?;

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

        // Core in 1.2, but still has to be asked for.
        let mut timeline =
            vk::PhysicalDeviceTimelineSemaphoreFeatures::default().timeline_semaphore(true);

        let queue_infos = [queue_info];
        let create_info = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queue_infos)
            .enabled_extension_names(&enabled_ptrs)
            .push_next(&mut timeline);

        let device = unsafe { instance.create_device(handle, &create_info, None) }
            .context("vkCreateDevice")?;
        let queue = unsafe { device.get_device_queue(queue_family, 0) };

        let has_timeline_semaphores = enabled.contains(&vk::KHR_TIMELINE_SEMAPHORE_NAME)
            || physical.api_version() >= Version::VERSION_1_2;

        tracing::info!(
            "vulkan device {} ({:?}), queue family {queue_family}",
            physical.name(),
            physical.ty()
        );

        Ok(Self {
            physical,
            device: Arc::new(device),
            queue,
            queue_family,
            has_timeline_semaphores,
        })
    }

    pub fn handle(&self) -> &Arc<ash::Device> {
        &self.device
    }

    pub fn physical(&self) -> &PhysicalDevice {
        &self.physical
    }

    pub fn queue(&self) -> vk::Queue {
        self.queue
    }

    pub fn queue_family(&self) -> u32 {
        self.queue_family
    }

    pub fn supports_timeline_semaphores(&self) -> bool {
        self.has_timeline_semaphores
    }

    pub fn name(&self) -> &str {
        self.physical.name()
    }
}

impl std::fmt::Debug for Device {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Device")
            .field("name", &self.physical.name())
            .field("queue_family", &self.queue_family)
            .field("timeline_semaphores", &self.has_timeline_semaphores)
            .finish()
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        unsafe {
            // Everything submitted has to have finished before the device goes
            // away; the alternative is a use-after-free inside the driver.
            let _ = self.device.device_wait_idle();
            self.device.destroy_device(None);
        }
    }
}
