// SPDX-License-Identifier: MIT
//
// Command pools and submission.
//
// A compositor records one command buffer per output per frame and throws it
// away, so the pool is created with RESET_COMMAND_BUFFER and buffers are
// re-recorded rather than freed and reallocated. Everything here is
// single-queue: this renderer composites, it does not run async compute, and a
// second queue would only add ownership transfers to pay for.

use std::os::fd::{FromRawFd, OwnedFd};
use std::time::Duration;

use anyhow::{anyhow, Context as _, Result};
use ash::vk;

use crate::Device;

/// A command pool and one reusable primary command buffer.
pub struct Commands {
    device: Device,
    pool: vk::CommandPool,
    buffer: vk::CommandBuffer,
    fence: vk::Fence,
    /// Signalled by every submission and exported as a sync_file, so the
    /// caller can hand the fd to KMS instead of waiting here.
    ///
    /// Exporting a SYNC_FD transfers the payload out, which leaves the
    /// semaphore unsignalled and ready for the next submit — so one is enough.
    signal: vk::Semaphore,
    /// Whether the fence has been submitted and not yet waited on. Resetting
    /// an unsignalled fence, or waiting on one that was never submitted, both
    /// hang rather than fail.
    pending: bool,
}

impl Commands {
    pub fn new(device: &Device) -> Result<Self> {
        let handle = device.handle();

        let pool_info = vk::CommandPoolCreateInfo::default()
            .queue_family_index(device.queue_family())
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        let pool = unsafe { handle.create_command_pool(&pool_info, None) }
            .context("vkCreateCommandPool")?;

        let allocate = vk::CommandBufferAllocateInfo::default()
            .command_pool(pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        let buffer = match unsafe { handle.allocate_command_buffers(&allocate) } {
            Ok(buffers) => buffers[0],
            Err(e) => {
                unsafe { handle.destroy_command_pool(pool, None) };
                return Err(anyhow::Error::from(e).context("vkAllocateCommandBuffers"));
            }
        };

        // Unsignalled: nothing has been submitted yet.
        let fence_info = vk::FenceCreateInfo::default();
        let fence = match unsafe { handle.create_fence(&fence_info, None) } {
            Ok(fence) => fence,
            Err(e) => {
                unsafe { handle.destroy_command_pool(pool, None) };
                return Err(anyhow::Error::from(e).context("vkCreateFence"));
            }
        };

        // Created up front as exportable; a semaphore cannot be made
        // exportable after the fact.
        let mut export = vk::ExportSemaphoreCreateInfo::default()
            .handle_types(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
        let semaphore_info = vk::SemaphoreCreateInfo::default().push_next(&mut export);
        let signal = match unsafe { handle.create_semaphore(&semaphore_info, None) } {
            Ok(semaphore) => semaphore,
            Err(e) => {
                unsafe {
                    handle.destroy_fence(fence, None);
                    handle.destroy_command_pool(pool, None);
                }
                return Err(anyhow::Error::from(e).context("vkCreateSemaphore"));
            }
        };

        Ok(Self {
            device: device.clone(),
            pool,
            buffer,
            fence,
            signal,
            pending: false,
        })
    }

    pub fn buffer(&self) -> vk::CommandBuffer {
        self.buffer
    }

    /// Start recording, waiting for the previous submission first.
    ///
    /// Re-recording a command buffer the GPU is still executing is undefined
    /// behaviour, and one of the few Vulkan mistakes that usually appears to
    /// work.
    pub fn begin(&mut self) -> Result<vk::CommandBuffer> {
        self.wait(Duration::from_secs(5))?;

        let handle = self.device.handle();
        unsafe {
            handle
                .reset_command_buffer(self.buffer, vk::CommandBufferResetFlags::empty())
                .context("vkResetCommandBuffer")?;
            let begin = vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
            handle
                .begin_command_buffer(self.buffer, &begin)
                .context("vkBeginCommandBuffer")?;
        }
        Ok(self.buffer)
    }

    /// Finish recording and submit.
    pub fn submit(&mut self) -> Result<()> {
        let handle = self.device.handle();
        unsafe {
            handle
                .end_command_buffer(self.buffer)
                .context("vkEndCommandBuffer")?;
            handle
                .reset_fences(&[self.fence])
                .context("vkResetFences")?;

            let buffers = [self.buffer];
            let signals = [self.signal];
            let submit = vk::SubmitInfo::default()
                .command_buffers(&buffers)
                .signal_semaphores(&signals);
            handle
                .queue_submit(self.device.queue(), &[submit], self.fence)
                .context("vkQueueSubmit")?;
        }
        self.pending = true;
        Ok(())
    }

    /// Export the last submission's completion as a `sync_file` fd.
    ///
    /// Must be called after [`Commands::submit`] and before the next one: the
    /// payload belongs to that submission, and exporting transfers it out.
    ///
    /// Returns `None` where the driver cannot export, which is not an error —
    /// the caller falls back to waiting.
    pub fn export_fence(&self) -> Result<Option<OwnedFd>> {
        if !self.pending {
            return Ok(None);
        }
        let info = vk::SemaphoreGetFdInfoKHR::default()
            .semaphore(self.signal)
            .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);

        let raw = unsafe { self.device.external_semaphore_fd().get_semaphore_fd(&info) }
            .context("vkGetSemaphoreFdKHR")?;

        // -1 is legal here: it means the work was already complete, so there
        // is nothing to wait on.
        if raw < 0 {
            return Ok(None);
        }
        if raw == 0 {
            return Err(anyhow!("vkGetSemaphoreFdKHR returned fd 0"));
        }
        Ok(Some(unsafe { OwnedFd::from_raw_fd(raw) }))
    }

    /// Block until the last submission completes.
    ///
    /// A real frame loop should not call this — the whole point of explicit
    /// sync is that the compositor hands a fence onward instead of waiting.
    /// It is here for teardown and for tests that need to read results back.
    pub fn wait(&mut self, timeout: Duration) -> Result<()> {
        if !self.pending {
            return Ok(());
        }
        let handle = self.device.handle();
        unsafe {
            handle
                .wait_for_fences(&[self.fence], true, timeout.as_nanos() as u64)
                .context("vkWaitForFences")?;
        }
        self.pending = false;
        Ok(())
    }
}

impl Drop for Commands {
    fn drop(&mut self) {
        // The pool cannot be destroyed while its buffer is still executing.
        let _ = self.wait(Duration::from_secs(5));

        let device = self.device.clone();
        let handle = device.handle();
        unsafe {
            handle.destroy_semaphore(self.signal, None);
            handle.destroy_fence(self.fence, None);
            handle.destroy_command_pool(self.pool, None);
        }
    }
}
