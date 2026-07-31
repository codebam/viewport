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

/// A linear NV12 buffer of `width` x `height`, built over one allocation.
///
/// gbm will not allocate NV12 — Mesa's implementation only does the
/// single-plane render formats — so a real decoder frame cannot be produced
/// from it. What can is the layout: one allocation holding both planes,
/// described the way a decoder describes it. That is precisely what the import
/// has to understand, and gbm was only ever standing in for it.
pub fn linear_nv12(
    allocator: &mut GbmAllocator<std::fs::File>,
    width: u32,
    height: u32,
) -> Option<smithay::backend::allocator::dmabuf::Dmabuf> {
    use smithay::backend::allocator::dmabuf::{AsDmabuf, Dmabuf, DmabufFlags};
    use smithay::backend::allocator::{Allocator, Fourcc, Modifier};

    // 4:2:0 chroma is half-height, so the two planes together are one and a
    // half times the picture. R8 because the bytes are all that matter here:
    // this is one allocation, and NV12 is how it gets read.
    let rows = height + height / 2;
    let backing = match allocator.create_buffer(width, rows, Fourcc::R8, &[Modifier::Linear]) {
        Ok(buffer) => buffer,
        Err(e) => {
            skip(&format!("gbm cannot allocate the backing store ({e})"));
            return None;
        }
    };
    let backing = backing.export().expect("export");
    let stride = backing.strides().next().expect("a stride");
    let fd = backing.handles().next().expect("an fd");

    let mut builder = Dmabuf::builder(
        (width as i32, height as i32),
        Fourcc::Nv12,
        Modifier::Linear,
        DmabufFlags::empty(),
    );
    // Luma first, the interleaved chroma directly after it — which is what
    // makes this NV12 rather than two unrelated planes.
    builder.add_plane(fd.try_clone_to_owned().expect("dup"), 0, stride);
    builder.add_plane(
        fd.try_clone_to_owned().expect("dup"),
        stride * height,
        stride,
    );
    Some(builder.build().expect("dmabuf builder"))
}

/// Fill an NV12 buffer with one colour, in the encoding the sampler expects.
///
/// `luma` is Y', `chroma` is (Cb, Cr) — narrow range, so 16..235 for luma and
/// 128 for neutral chroma.
pub fn fill_nv12(
    buffer: &smithay::backend::allocator::dmabuf::Dmabuf,
    height: u32,
    luma: u8,
    chroma: (u8, u8),
) {
    use smithay::backend::allocator::dmabuf::{DmabufMappingMode, DmabufSyncFlags};
    let stride = buffer.strides().next().expect("stride") as usize;
    let chroma_offset = buffer.offsets().nth(1).expect("a second plane") as usize;

    buffer
        .sync_plane(0, DmabufSyncFlags::START | DmabufSyncFlags::WRITE)
        .expect("sync start");
    let mapping = buffer
        .map_plane(0, DmabufMappingMode::WRITE)
        .expect("map write");
    // SAFETY: the mapping is valid and writable for its own length, and
    // nothing else holds it.
    let bytes =
        unsafe { std::slice::from_raw_parts_mut(mapping.ptr() as *mut u8, mapping.length()) };

    for row in 0..height as usize {
        let at = row * stride;
        bytes[at..at + stride].fill(luma);
    }
    // Half the rows, and Cb and Cr interleaved along each of them.
    for row in 0..(height as usize) / 2 {
        let at = chroma_offset + row * stride;
        for pair in bytes[at..at + stride].chunks_exact_mut(2) {
            pair[0] = chroma.0;
            pair[1] = chroma.1;
        }
    }

    drop(mapping);
    let _ = buffer.sync_plane(0, DmabufSyncFlags::END | DmabufSyncFlags::WRITE);
}
