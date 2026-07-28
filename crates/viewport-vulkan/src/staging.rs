// SPDX-License-Identifier: MIT
//
// Host-visible buffers, for getting pixels from the CPU onto the GPU.
//
// Only shm clients need this. A DMA-BUF is already GPU memory and is imported
// in place; an shm buffer is a chunk of shared memory the client wrote with
// the CPU, so it has to be copied. That copy is the reason a compositor
// prefers dmabuf clients, and the reason this path exists at all: plenty of
// Wayland clients only ever use shm.

use anyhow::{anyhow, Context as _, Result};
use ash::vk;

use crate::Device;

/// A host-visible, host-coherent buffer used as a copy source.
///
/// Coherent rather than manually flushed: the writes here are one memcpy
/// immediately before a submit, so the flush would cover the whole range
/// anyway and coherent memory removes a step that is easy to forget.
pub struct Staging {
    device: Device,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    mapped: *mut u8,
    size: vk::DeviceSize,
}

impl Staging {
    pub fn new(device: &Device, size: vk::DeviceSize) -> Result<Self> {
        anyhow::ensure!(size > 0, "a zero-sized staging buffer");
        let handle = device.handle();

        let info = vk::BufferCreateInfo::default()
            .size(size)
            .usage(vk::BufferUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let buffer = unsafe { handle.create_buffer(&info, None) }.context("vkCreateBuffer")?;

        let result = (|| -> Result<(vk::DeviceMemory, *mut u8)> {
            let requirements = unsafe { handle.get_buffer_memory_requirements(buffer) };
            let memory_type = device
                .memory_type_with(requirements.memory_type_bits, |flags| {
                    flags.contains(vk::MemoryPropertyFlags::HOST_VISIBLE)
                        && flags.contains(vk::MemoryPropertyFlags::HOST_COHERENT)
                })
                .ok_or_else(|| anyhow!("no host-visible coherent memory type"))?;

            let allocate = vk::MemoryAllocateInfo::default()
                .allocation_size(requirements.size)
                .memory_type_index(memory_type);
            let memory = unsafe { handle.allocate_memory(&allocate, None) }
                .context("vkAllocateMemory")?;

            if let Err(e) = unsafe { handle.bind_buffer_memory(buffer, memory, 0) } {
                unsafe { handle.free_memory(memory, None) };
                return Err(anyhow::Error::from(e).context("vkBindBufferMemory"));
            }

            // Mapped once and left mapped. Repeatedly mapping and unmapping a
            // buffer that is written every frame is pure overhead.
            let mapped = match unsafe {
                handle.map_memory(memory, 0, requirements.size, vk::MemoryMapFlags::empty())
            } {
                Ok(ptr) => ptr as *mut u8,
                Err(e) => {
                    unsafe { handle.free_memory(memory, None) };
                    return Err(anyhow::Error::from(e).context("vkMapMemory"));
                }
            };

            Ok((memory, mapped))
        })();

        let (memory, mapped) = match result {
            Ok(pair) => pair,
            Err(e) => {
                unsafe { handle.destroy_buffer(buffer, None) };
                return Err(e);
            }
        };

        Ok(Self {
            device: device.clone(),
            buffer,
            memory,
            mapped,
            size,
        })
    }

    pub fn handle(&self) -> vk::Buffer {
        self.buffer
    }

    pub fn size(&self) -> vk::DeviceSize {
        self.size
    }

    /// Copy `data` in at `offset`.
    pub fn write(&mut self, offset: usize, data: &[u8]) -> Result<()> {
        let end = offset
            .checked_add(data.len())
            .ok_or_else(|| anyhow!("staging write overflows"))?;
        anyhow::ensure!(
            end as vk::DeviceSize <= self.size,
            "staging write of {} bytes at {offset} exceeds the {} byte buffer",
            data.len(),
            self.size
        );

        // SAFETY: the range is bounds-checked above, the memory is mapped for
        // the lifetime of this struct, and it is host-coherent so no flush is
        // needed before the GPU reads it.
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), self.mapped.add(offset), data.len());
        }
        Ok(())
    }
}

impl std::fmt::Debug for Staging {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Staging").field("size", &self.size).finish()
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let handle = self.device.handle();
        unsafe {
            // A copy may still be reading it.
            let _ = handle.device_wait_idle();
            handle.unmap_memory(self.memory);
            handle.destroy_buffer(self.buffer, None);
            handle.free_memory(self.memory, None);
        }
    }
}
