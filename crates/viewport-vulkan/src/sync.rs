// SPDX-License-Identifier: MIT
//
// Fences, as file descriptors.
//
// The whole point of explicit sync is that nobody blocks. When a frame is
// submitted the compositor should get back something it can hand to KMS — a
// sync_file the display controller waits on itself — rather than a promise
// that the CPU already sat and waited for the GPU to finish.
//
// A sync_file is just a pollable fd: it becomes readable when the work it
// represents completes. That is why the same fd works for KMS, for
// drm_syncobj, for another process, and for the `Fence` trait here.

use std::os::fd::{AsRawFd, OwnedFd};

use smithay::backend::renderer::sync::{Fence, Interrupted};

/// A `sync_file` fd exported from a Vulkan submission.
#[derive(Debug)]
pub struct SyncFile(OwnedFd);

impl SyncFile {
    pub fn new(fd: OwnedFd) -> Self {
        Self(fd)
    }

    pub fn as_fd(&self) -> &OwnedFd {
        &self.0
    }

    /// Poll the fd. `timeout` is in milliseconds; -1 blocks.
    fn poll(&self, timeout: i32) -> std::io::Result<bool> {
        let mut poll = libc::pollfd {
            fd: self.0.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        loop {
            let ready = unsafe { libc::poll(&mut poll, 1, timeout) };
            if ready >= 0 {
                return Ok(ready > 0);
            }
            let error = std::io::Error::last_os_error();
            // A signal arriving is not the fence failing.
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}

impl Fence for SyncFile {
    fn is_signaled(&self) -> bool {
        // A zero timeout asks "is it ready now" without waiting.
        self.poll(0).unwrap_or(false)
    }

    fn wait(&self) -> Result<(), Interrupted> {
        self.poll(-1).map(|_| ()).map_err(|_| Interrupted)
    }

    fn is_exportable(&self) -> bool {
        true
    }

    fn export(&self) -> Option<OwnedFd> {
        // Duplicated, because the caller takes ownership of what it gets and
        // this fence may still be waited on here.
        self.0.try_clone().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::FromRawFd;

    /// An eventfd stands in for a sync_file: both are pollable, and one that
    /// has been written to is readable in the same way a signalled fence is.
    fn eventfd(initial: u32) -> OwnedFd {
        let raw = unsafe { libc::eventfd(initial, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        assert!(raw >= 0, "eventfd: {}", std::io::Error::last_os_error());
        unsafe { OwnedFd::from_raw_fd(raw) }
    }

    #[test]
    fn an_unsignalled_fence_is_not_reported_as_reached() {
        let fence = SyncFile::new(eventfd(0));
        assert!(!fence.is_signaled());
    }

    #[test]
    fn a_signalled_fence_is_reported_immediately() {
        let fence = SyncFile::new(eventfd(1));
        assert!(fence.is_signaled());
        // And waiting on it returns rather than blocking the test.
        fence.wait().expect("wait");
    }

    #[test]
    fn exporting_gives_an_independent_fd() {
        let fence = SyncFile::new(eventfd(1));
        let exported = fence.export().expect("export");
        // A different fd number for the same open file: the caller can close
        // theirs without signalling ours away.
        assert_ne!(exported.as_raw_fd(), fence.as_fd().as_raw_fd());
        assert!(fence.is_signaled());
    }
}
