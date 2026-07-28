// SPDX-License-Identifier: MIT
//
// A Vulkan renderer for Smithay.
//
// Smithay ships GLES, glow and pixman renderers and no Vulkan one — its
// `backend::vulkan` module says outright that it "does not provide
// abstractions for logical devices, rendering or memory allocation". wlroots
// has had a Vulkan renderer for years and the C build of Viewport uses it, so
// this exists to close that gap.
//
// Deliberately free of any Viewport-specific type: it takes Smithay's `Dmabuf`
// and implements Smithay's renderer traits, so it is a general Smithay
// renderer that happens to live here. That is for reusability, not for
// upstreaming — Smithay's AI.md rules this code out of their tree regardless
// of how it is licensed.
//
// ## Why bother, when GLES works
//
// Colour. Viewport composites HDR clients next to SDR ones, which means every
// surface needs a transfer function and primaries applied on the way to a
// shared output space. In GLES that is a fragment shader per combination and a
// lot of driver-specific guesswork about what the texture actually contains.
// In Vulkan the format, modifier and colour space of an imported image are
// stated explicitly, which is what makes `color-management-v1` implementable
// rather than approximable.

pub mod command;
pub mod device;
pub mod format;
pub mod image;
pub mod pipeline;
pub mod render;
pub mod staging;
pub mod sync;
pub mod transform;
pub mod renderer;

#[cfg(test)]
mod test_support;

pub use command::Commands;
pub use device::Device;
pub use image::{Image, Purpose};
pub use pipeline::Pipelines;
pub use render::{Color, Frame, Rect};
pub use renderer::{VulkanFramebuffer, VulkanMapping, VulkanRenderer, VulkanTexture};

use anyhow::{Context as _, Result};
use smithay::backend::drm::DrmNode;
use smithay::backend::vulkan::{version::Version, Instance};

/// Open a Vulkan device on the GPU behind `node`.
///
/// The convenience entry point: it creates an instance, finds the device
/// exposing that DRM node, and opens it. The returned [`Device`] keeps the
/// instance alive, so there is nothing else to hold on to.
///
/// Use [`Device::for_node`] instead where the instance is already owned —
/// a compositor sharing one instance across several GPUs, for example.
pub fn open(node: &DrmNode) -> Result<Device> {
    let instance = Instance::new(Version::VERSION_1_3, None).context("vkCreateInstance")?;
    Device::for_node(&instance, node)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// As in `viewport-web`: skipping and passing look identical from the
    /// outside, so `VIEWPORT_REQUIRE_GPU=1` turns a skip into a failure. CI
    /// sets it.
    fn require() -> bool {
        std::env::var("VIEWPORT_REQUIRE_GPU").is_ok_and(|v| v == "1")
    }

    fn skip(reason: &str) -> bool {
        assert!(!require(), "VIEWPORT_REQUIRE_GPU=1 but {reason}");
        eprintln!("{reason}; skipping");
        true
    }

    fn node() -> Option<DrmNode> {
        let path = Path::new("/dev/dri/renderD128");
        if !path.exists() {
            skip("there is no /dev/dri/renderD128");
            return None;
        }
        match DrmNode::from_path(path) {
            Ok(node) => Some(node),
            Err(e) => {
                skip(&format!("could not open the render node ({e})"));
                None
            }
        }
    }

    #[test]
    fn a_device_can_be_opened_on_the_render_node() {
        let Some(node) = node() else { return };
        let device = match open(&node) {
            Ok(device) => device,
            Err(e) => {
                skip(&format!("no usable Vulkan device ({e})"));
                return;
            }
        };

        assert!(!device.name().is_empty());
        assert_ne!(device.queue(), ash::vk::Queue::null());
        // 1.2 is the floor, and timeline semaphores are core there.
        assert!(
            device.supports_timeline_semaphores(),
            "a 1.2 device without timeline semaphores"
        );
    }

    #[test]
    fn the_device_matches_the_node_it_was_asked_for() {
        // The whole point of selecting by node: importing a client's DMA-BUF
        // into a device on a different card fails or copies over PCIe.
        let Some(node) = node() else { return };
        let Ok(device) = open(&node) else {
            skip("no usable Vulkan device");
            return;
        };

        let render = device.physical().render_node().ok().flatten();
        let primary = device.physical().primary_node().ok().flatten();
        assert!(
            render == Some(node) || primary == Some(node),
            "opened {:?}/{:?}, asked for {node:?}",
            render,
            primary
        );
    }
}
