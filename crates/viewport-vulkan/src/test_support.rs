// SPDX-License-Identifier: MIT
//
// Shared test scaffolding.
//
// The GPU tests skip where there is no render node, and a skipped test is
// indistinguishable from a passing one in the output. `VIEWPORT_REQUIRE_GPU=1`
// turns every skip into a failure so CI cannot report a green run on a machine
// that never touched a GPU — which has already happened once in this project.

use std::path::Path;

use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
use smithay::backend::drm::DrmNode;

use crate::Device;

pub const RENDER_NODE: &str = "/dev/dri/renderD128";

pub struct TestGpu {
    pub device: Device,
    pub node: DrmNode,
}

fn required() -> bool {
    std::env::var("VIEWPORT_REQUIRE_GPU").is_ok_and(|v| v == "1")
}

pub fn skip(reason: &str) {
    assert!(!required(), "VIEWPORT_REQUIRE_GPU=1 but {reason}");
    eprintln!("{reason}; skipping");
}

/// An open Vulkan device on the render node, or `None` where there is none.
pub fn require_gpu() -> Option<TestGpu> {
    if !Path::new(RENDER_NODE).exists() {
        skip(&format!("there is no {RENDER_NODE}"));
        return None;
    }
    let node = match DrmNode::from_path(RENDER_NODE) {
        Ok(node) => node,
        Err(e) => {
            skip(&format!("could not open {RENDER_NODE} ({e})"));
            return None;
        }
    };
    match crate::open(&node) {
        Ok(device) => Some(TestGpu { device, node }),
        Err(e) => {
            skip(&format!("no usable Vulkan device ({e})"));
            None
        }
    }
}

/// A GBM allocator on the same node.
///
/// Buffers come from GBM rather than from Vulkan on purpose: that is how they
/// arrive in practice — a client or the web engine allocated them with an
/// entirely different API — so importing a GBM buffer is the case worth
/// testing, not a Vulkan image round-tripped through itself.
pub fn gbm_allocator(node: &DrmNode) -> Option<GbmAllocator<std::fs::File>> {
    let path = node.dev_path().unwrap_or_else(|| RENDER_NODE.into());
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
    {
        Ok(file) => file,
        Err(e) => {
            skip(&format!("could not open {} ({e})", path.display()));
            return None;
        }
    };
    match GbmDevice::new(file) {
        Ok(device) => Some(GbmAllocator::new(device, GbmBufferFlags::RENDERING)),
        Err(e) => {
            skip(&format!("gbm_create_device failed ({e})"));
            None
        }
    }
}
