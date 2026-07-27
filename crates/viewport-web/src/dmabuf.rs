// SPDX-License-Identifier: MIT
//
// The buffer the shell is painted into.
//
// This is the piece WPE gave us for free and Servo does not. `WPEBufferDMABuf`
// handed the compositor a DMA-BUF per frame; Servo's built-in rendering
// contexts are all surfman-backed and none of them exports one. So we allocate
// the buffer ourselves with GBM, import it into EGL as an `EGLImage`, hang it
// off a GL framebuffer, and let Servo render into that framebuffer — which is
// exactly what `RenderingContext::prepare_for_rendering` exists to allow.
//
// Nothing here depends on Servo. It is the buffer half of the problem, kept
// separate so it can be tested on its own — and so it stays usable if the
// engine underneath ever changes again.

use std::ffi::c_void;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, Context as _, Result};
use drm_fourcc::{DrmFourcc, DrmModifier};
use gbm::{AsRaw, BufferObject, BufferObjectFlags, Device as GbmDevice};

/// `EGL_PLATFORM_GBM_KHR`
const PLATFORM_GBM: khronos_egl::Enum = 0x31D7;
/// `EGL_LINUX_DMA_BUF_EXT`
const LINUX_DMA_BUF: khronos_egl::Enum = 0x3270;

const LINUX_DRM_FOURCC: i32 = 0x3271;
const DMA_BUF_PLANE0_FD: i32 = 0x3272;
const DMA_BUF_PLANE0_OFFSET: i32 = 0x3273;
const DMA_BUF_PLANE0_PITCH: i32 = 0x3274;
const DMA_BUF_PLANE0_MODIFIER_LO: i32 = 0x3443;
const DMA_BUF_PLANE0_MODIFIER_HI: i32 = 0x3444;
const EGL_WIDTH: i32 = 0x3057;
const EGL_HEIGHT: i32 = 0x3056;
const EGL_NONE_ATTRIB: i32 = 0x3038;

/// `EGL_SYNC_NATIVE_FENCE_ANDROID`
const SYNC_NATIVE_FENCE: khronos_egl::Enum = 0x3144;
/// `EGL_NO_NATIVE_FENCE_FD_ANDROID`
const NO_NATIVE_FENCE_FD: i32 = -1;

type EglGetPlatformDisplay =
    unsafe extern "C" fn(khronos_egl::Enum, *mut c_void, *const isize) -> *mut c_void;
type EglCreateImageKhr = unsafe extern "C" fn(
    *mut c_void,
    *mut c_void,
    khronos_egl::Enum,
    *mut c_void,
    *const i32,
) -> *mut c_void;
type EglDestroyImageKhr = unsafe extern "C" fn(*mut c_void, *mut c_void) -> u32;
type GlEglImageTargetTexture2dOes = unsafe extern "C" fn(u32, *mut c_void);
type EglCreateSyncKhr =
    unsafe extern "C" fn(*mut c_void, khronos_egl::Enum, *const i32) -> *mut c_void;
type EglDestroySyncKhr = unsafe extern "C" fn(*mut c_void, *mut c_void) -> u32;
type EglDupNativeFenceFdAndroid = unsafe extern "C" fn(*mut c_void, *mut c_void) -> i32;

/// A DMA-BUF and everything needed to import it somewhere else.
///
/// This is what crosses the boundary to the compositor. The fd is the buffer;
/// the rest is how to interpret it.
#[derive(Debug)]
pub struct Dmabuf {
    pub fd: OwnedFd,
    pub width: u32,
    pub height: u32,
    pub format: DrmFourcc,
    pub modifier: DrmModifier,
    pub stride: u32,
    pub offset: u32,
}

/// A GPU: one GBM device, one EGL display, one context.
///
/// Servo renders on this; the compositor imports what comes out of it. The two
/// need not be the same `Gpu` — that they are not is the whole point, and the
/// test at the bottom of this file proves it by rendering on one and reading
/// back on another.
pub struct Gpu {
    egl: Arc<khronos_egl::DynamicInstance<khronos_egl::EGL1_5>>,
    display: khronos_egl::Display,
    context: khronos_egl::Context,
    gl: Arc<glow::Context>,

    create_image: EglCreateImageKhr,
    destroy_image: EglDestroyImageKhr,
    image_target_texture: GlEglImageTargetTexture2dOes,

    /// Present only where `EGL_ANDROID_native_fence_sync` is. Without it the
    /// compositor has to fall back to a CPU wait, which works and is slower.
    fence_functions: Option<FenceFunctions>,

    // Dropped last: the buffer objects borrow it.
    gbm: GbmDevice<std::fs::File>,
}

impl Gpu {
    /// Open a render node. `/dev/dri/renderD128` on most systems.
    ///
    /// A render node rather than a card node on purpose: it needs no DRM
    /// master, so the web engine can allocate buffers without any say over the
    /// display, and it works with no session at all — which is what makes this
    /// testable in CI.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("open {}", path.display()))?;
        let gbm = GbmDevice::new(file).context("gbm_create_device")?;

        let egl = Arc::new(
            unsafe { khronos_egl::DynamicInstance::<khronos_egl::EGL1_5>::load_required() }
                .map_err(|e| anyhow!("load libEGL: {e}"))?,
        );

        // eglGetPlatformDisplay over the GBM device, rather than a default
        // display: there is no X server or Wayland compositor to talk to here,
        // and the buffers have to come from this specific device anyway.
        let get_platform_display: EglGetPlatformDisplay = unsafe {
            std::mem::transmute(
                egl.get_proc_address("eglGetPlatformDisplayEXT")
                    .or_else(|| egl.get_proc_address("eglGetPlatformDisplay"))
                    .ok_or_else(|| anyhow!("no eglGetPlatformDisplay"))?,
            )
        };
        let display_ptr = unsafe {
            get_platform_display(
                PLATFORM_GBM,
                gbm.as_raw_mut() as *mut c_void,
                std::ptr::null(),
            )
        };
        if display_ptr.is_null() {
            return Err(anyhow!("eglGetPlatformDisplay returned no display"));
        }
        let display = unsafe { khronos_egl::Display::from_ptr(display_ptr) };

        let (major, minor) = egl.initialize(display).context("eglInitialize")?;
        tracing::debug!("EGL {major}.{minor} on {}", path.display());

        let extensions = egl
            .query_string(Some(display), khronos_egl::EXTENSIONS)
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        for required in [
            "EGL_KHR_image_base",
            "EGL_EXT_image_dma_buf_import",
            "EGL_KHR_surfaceless_context",
        ] {
            if !extensions.contains(required) {
                return Err(anyhow!("{} does not support {required}", path.display()));
            }
        }
        // Only needed to allocate with an explicit modifier list; without it we
        // take whatever GBM picks and pass the modifier along.
        let has_modifiers = extensions.contains("EGL_EXT_image_dma_buf_import_modifiers");

        egl.bind_api(khronos_egl::OPENGL_ES_API)
            .context("eglBindAPI")?;

        // No surface is ever created, so no surface type is asked for.
        //
        // A GBM platform display advertises window configs matching the
        // device's scanout formats and no pbuffer configs at all, so
        // constraining SURFACE_TYPE here finds nothing. What the context
        // actually needs is to be ES3-renderable and nothing else.
        let config = match egl
            .choose_first_config(
                display,
                &[
                    khronos_egl::RENDERABLE_TYPE,
                    khronos_egl::OPENGL_ES3_BIT,
                    khronos_egl::NONE,
                ],
            )
            .context("eglChooseConfig")?
        {
            Some(config) => config,
            // EGL_NO_CONFIG_KHR. The context is only ever made current against
            // framebuffer objects, so it has no default framebuffer to
            // describe and does not need a config to describe it with.
            None if extensions.contains("EGL_KHR_no_config_context") => unsafe {
                khronos_egl::Config::from_ptr(std::ptr::null_mut())
            },
            None => return Err(anyhow!("no ES3-renderable EGL config and no_config_context is unsupported")),
        };

        let context = egl
            .create_context(
                display,
                config,
                None,
                &[khronos_egl::CONTEXT_MAJOR_VERSION, 3, khronos_egl::NONE],
            )
            .context("eglCreateContext")?;

        // Surfaceless: everything is rendered to a framebuffer object.
        egl.make_current(display, None, None, Some(context))
            .context("eglMakeCurrent")?;

        let gl = Arc::new(unsafe {
            let egl = egl.clone();
            glow::Context::from_loader_function(move |name| {
                egl.get_proc_address(name)
                    .map_or(std::ptr::null(), |p| p as *const c_void)
            })
        });

        let create_image: EglCreateImageKhr = unsafe {
            std::mem::transmute(
                egl.get_proc_address("eglCreateImageKHR")
                    .ok_or_else(|| anyhow!("no eglCreateImageKHR"))?,
            )
        };
        let destroy_image: EglDestroyImageKhr = unsafe {
            std::mem::transmute(
                egl.get_proc_address("eglDestroyImageKHR")
                    .ok_or_else(|| anyhow!("no eglDestroyImageKHR"))?,
            )
        };
        let image_target_texture: GlEglImageTargetTexture2dOes = unsafe {
            std::mem::transmute(
                egl.get_proc_address("glEGLImageTargetTexture2DOES")
                    .ok_or_else(|| anyhow!("no glEGLImageTargetTexture2DOES"))?,
            )
        };

        // The fence is what makes this asynchronous. WebKit attached one to
        // every frame and the compositor waited on it in the scene graph
        // instead of blocking; the same has to be true here or the win from
        // zero-copy is given straight back in stalls.
        let fence_functions = if extensions.contains("EGL_ANDROID_native_fence_sync") {
            match (
                egl.get_proc_address("eglCreateSyncKHR"),
                egl.get_proc_address("eglDestroySyncKHR"),
                egl.get_proc_address("eglDupNativeFenceFDANDROID"),
            ) {
                (Some(create), Some(destroy), Some(dup)) => unsafe {
                    Some(FenceFunctions {
                        create_sync: std::mem::transmute::<_, EglCreateSyncKhr>(create),
                        destroy_sync: std::mem::transmute::<_, EglDestroySyncKhr>(destroy),
                        dup_fd: std::mem::transmute::<_, EglDupNativeFenceFdAndroid>(dup),
                    })
                },
                _ => None,
            }
        } else {
            None
        };

        tracing::debug!(
            "dma-buf import modifiers: {has_modifiers}, native fences: {}",
            fence_functions.is_some()
        );

        Ok(Self {
            egl,
            display,
            context,
            gl,
            create_image,
            destroy_image,
            image_target_texture,
            fence_functions,
            gbm,
        })
    }

    /// Whether this GPU can hand out fence fds.
    pub fn supports_fences(&self) -> bool {
        self.fence_functions.is_some()
    }

    /// Take a fence for the work submitted so far, as a syncobj-importable fd.
    ///
    /// Call this after issuing the frame's draw calls and before handing the
    /// buffer over. The compositor imports the fd into a `drm_syncobj` timeline
    /// and lets the scene wait on that point, so nothing on the CPU blocks.
    ///
    /// Returns `None` where the driver has no native fence support. That is a
    /// missed optimisation, not an error — the caller falls back to `glFinish`.
    pub fn fence(&self) -> Result<Option<OwnedFd>> {
        use std::os::fd::FromRawFd;

        let Some(functions) = self.fence_functions.as_ref() else {
            return Ok(None);
        };

        let sync = unsafe {
            (functions.create_sync)(
                self.display.as_ptr(),
                SYNC_NATIVE_FENCE,
                [EGL_NONE_ATTRIB].as_ptr(),
            )
        };
        if sync.is_null() {
            return Err(anyhow!("eglCreateSyncKHR failed"));
        }

        // The fence is only submitted to the GPU on a flush; without this the
        // dup below can return a fd for work the driver has not sent yet.
        use glow::HasContext;
        unsafe {
            self.gl.flush();
        }

        let raw = unsafe { (functions.dup_fd)(self.display.as_ptr(), sync) };
        unsafe {
            (functions.destroy_sync)(self.display.as_ptr(), sync);
        }

        if raw == NO_NATIVE_FENCE_FD {
            return Err(anyhow!("eglDupNativeFenceFDANDROID returned no fd"));
        }
        Ok(Some(unsafe { OwnedFd::from_raw_fd(raw) }))
    }

    pub fn gl(&self) -> &Arc<glow::Context> {
        &self.gl
    }

    pub fn make_current(&self) -> Result<()> {
        self.egl
            .make_current(self.display, None, None, Some(self.context))
            .context("eglMakeCurrent")
    }

    /// Allocate a buffer and export it as a DMA-BUF.
    ///
    /// `SCANOUT` is deliberately not requested: this buffer is composited, not
    /// handed to KMS directly, and asking for scanout narrows the modifiers the
    /// driver will pick for no gain.
    pub fn allocate(&self, width: u32, height: u32) -> Result<Dmabuf> {
        let bo: BufferObject<()> = self
            .gbm
            .create_buffer_object(width, height, gbm::Format::Argb8888, BufferObjectFlags::RENDERING)
            .context("gbm_bo_create")?;

        // Multi-plane buffers would need every plane's fd; nothing allocated
        // here is planar, and treating one as if it were would silently drop
        // the other planes.
        let planes = bo.plane_count();
        if planes != 1 {
            return Err(anyhow!("expected a single-plane buffer, got {planes}"));
        }

        Ok(Dmabuf {
            fd: bo.fd().context("gbm_bo_get_fd")?,
            width,
            height,
            format: DrmFourcc::Argb8888,
            modifier: DrmModifier::from(u64::from(bo.modifier())),
            stride: bo.stride(),
            offset: bo.offset(0),
        })
    }

    /// Import a DMA-BUF as a GL texture on *this* GPU.
    ///
    /// The buffer need not have been allocated here — that is the point of a
    /// DMA-BUF, and it is what lets Servo render on one context and the
    /// compositor sample on another without a copy.
    pub fn import(&self, buffer: &Dmabuf) -> Result<Texture> {
        let modifier = u64::from(buffer.modifier);
        let attributes: [i32; 17] = [
            EGL_WIDTH,
            buffer.width as i32,
            EGL_HEIGHT,
            buffer.height as i32,
            LINUX_DRM_FOURCC,
            buffer.format as i32,
            DMA_BUF_PLANE0_FD,
            buffer.fd.as_raw_fd(),
            DMA_BUF_PLANE0_OFFSET,
            buffer.offset as i32,
            DMA_BUF_PLANE0_PITCH,
            buffer.stride as i32,
            DMA_BUF_PLANE0_MODIFIER_LO,
            (modifier & 0xFFFF_FFFF) as i32,
            DMA_BUF_PLANE0_MODIFIER_HI,
            (modifier >> 32) as i32,
            EGL_NONE_ATTRIB,
        ];

        // EGL_NO_CONTEXT: a dma_buf image belongs to the display, not to any
        // one context, which is exactly why it can be shared.
        let image = unsafe {
            (self.create_image)(
                self.display.as_ptr(),
                khronos_egl::NO_CONTEXT,
                LINUX_DMA_BUF,
                std::ptr::null_mut(),
                attributes.as_ptr(),
            )
        };
        if image.is_null() {
            return Err(anyhow!(
                "eglCreateImageKHR failed for {}x{} {:?} modifier {modifier:#x}",
                buffer.width,
                buffer.height,
                buffer.format
            ));
        }

        use glow::HasContext;
        let texture = unsafe {
            let texture = self
                .gl
                .create_texture()
                .map_err(|e| anyhow!("glGenTextures: {e}"))?;
            self.gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            self.gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MIN_FILTER,
                glow::LINEAR as i32,
            );
            self.gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MAG_FILTER,
                glow::LINEAR as i32,
            );
            (self.image_target_texture)(glow::TEXTURE_2D, image);
            self.gl.bind_texture(glow::TEXTURE_2D, None);
            texture
        };

        Ok(Texture {
            image,
            texture,
            gl: self.gl.clone(),
            display: self.display.as_ptr(),
            destroy_image: self.destroy_image,
        })
    }

    /// A framebuffer over an imported buffer, for something else to draw into.
    ///
    /// This is what gets bound in `RenderingContext::prepare_for_rendering`:
    /// Servo believes it is drawing to the screen and is in fact drawing
    /// straight into the buffer the compositor will scan out.
    pub fn framebuffer(&self, texture: &Texture) -> Result<Framebuffer> {
        use glow::HasContext;
        unsafe {
            let fbo = self
                .gl
                .create_framebuffer()
                .map_err(|e| anyhow!("glGenFramebuffers: {e}"))?;
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            self.gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(texture.texture),
                0,
            );
            let status = self.gl.check_framebuffer_status(glow::FRAMEBUFFER);
            if status != glow::FRAMEBUFFER_COMPLETE {
                self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
                self.gl.delete_framebuffer(fbo);
                return Err(anyhow!("framebuffer incomplete: {status:#x}"));
            }
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            Ok(Framebuffer {
                fbo,
                gl: self.gl.clone(),
            })
        }
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        let _ = self.egl.make_current(self.display, None, None, None);
        let _ = self.egl.destroy_context(self.display, self.context);
        let _ = self.egl.terminate(self.display);
    }
}

struct FenceFunctions {
    create_sync: EglCreateSyncKhr,
    destroy_sync: EglDestroySyncKhr,
    dup_fd: EglDupNativeFenceFdAndroid,
}

/// An imported DMA-BUF, as a GL texture.
pub struct Texture {
    image: *mut c_void,
    texture: glow::Texture,
    gl: Arc<glow::Context>,
    display: khronos_egl::EGLDisplay,
    destroy_image: EglDestroyImageKhr,
}

impl Texture {
    pub fn raw(&self) -> glow::Texture {
        self.texture
    }
}

impl Drop for Texture {
    fn drop(&mut self) {
        use glow::HasContext;
        unsafe {
            self.gl.delete_texture(self.texture);
            (self.destroy_image)(self.display, self.image);
        }
    }
}

pub struct Framebuffer {
    fbo: glow::Framebuffer,
    gl: Arc<glow::Context>,
}

impl Framebuffer {
    pub fn raw(&self) -> glow::Framebuffer {
        self.fbo
    }

    pub fn bind(&self) {
        use glow::HasContext;
        unsafe {
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.fbo));
        }
    }
}

impl Drop for Framebuffer {
    fn drop(&mut self) {
        use glow::HasContext;
        unsafe {
            self.gl.delete_framebuffer(self.fbo);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glow::HasContext;

    /// Skip rather than fail where there is no GPU. A machine without a render
    /// node cannot answer the question these tests ask.
    ///
    /// Set `VIEWPORT_REQUIRE_GPU=1` to turn every skip into a failure. Without
    /// it these tests pass on a machine where EGL will not even load, which
    /// looks exactly like the buffer sharing working — the one result that must
    /// never be reported by accident. CI sets it.
    fn gpu() -> Option<Gpu> {
        let required = std::env::var("VIEWPORT_REQUIRE_GPU").is_ok_and(|v| v == "1");

        let skip = |reason: String| -> Option<Gpu> {
            assert!(!required, "VIEWPORT_REQUIRE_GPU=1 but {reason}");
            eprintln!("{reason}; skipping");
            None
        };

        if !Path::new("/dev/dri/renderD128").exists() {
            return skip("there is no /dev/dri/renderD128".to_owned());
        }
        match Gpu::open("/dev/dri/renderD128") {
            Ok(gpu) => Some(gpu),
            Err(e) => skip(format!("could not open the render node ({e})")),
        }
    }

    #[test]
    fn a_buffer_can_be_allocated_and_described() {
        let Some(gpu) = gpu() else { return };
        let buffer = gpu.allocate(256, 128).expect("allocate");

        assert_eq!(buffer.width, 256);
        assert_eq!(buffer.height, 128);
        assert_eq!(buffer.format, DrmFourcc::Argb8888);
        // Four bytes per pixel, and the driver may pad the row.
        assert!(
            buffer.stride >= 256 * 4,
            "stride {} is narrower than a row",
            buffer.stride
        );
        assert!(buffer.fd.as_raw_fd() >= 0);
    }

    #[test]
    fn a_buffer_can_be_rendered_into_through_a_framebuffer() {
        let Some(gpu) = gpu() else { return };
        let buffer = gpu.allocate(64, 64).expect("allocate");
        let texture = gpu.import(&buffer).expect("import");
        let framebuffer = gpu.framebuffer(&texture).expect("framebuffer");

        framebuffer.bind();
        unsafe {
            gpu.gl().viewport(0, 0, 64, 64);
            gpu.gl().clear_color(0.0, 1.0, 0.0, 1.0);
            gpu.gl().clear(glow::COLOR_BUFFER_BIT);
            gpu.gl().finish();
        }

        let mut pixels = [0u8; 4];
        unsafe {
            gpu.gl().read_pixels(
                0,
                0,
                1,
                1,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(Some(&mut pixels)),
            );
        }
        assert_eq!(pixels, [0, 255, 0, 255], "framebuffer did not take the clear");
    }

    #[test]
    fn a_fence_can_be_taken_for_submitted_work() {
        let Some(gpu) = gpu() else { return };
        if !gpu.supports_fences() {
            eprintln!("no EGL_ANDROID_native_fence_sync; skipping");
            return;
        }

        let buffer = gpu.allocate(64, 64).expect("allocate");
        let texture = gpu.import(&buffer).expect("import");
        let framebuffer = gpu.framebuffer(&texture).expect("framebuffer");
        framebuffer.bind();
        unsafe {
            gpu.gl().viewport(0, 0, 64, 64);
            gpu.gl().clear_color(0.0, 0.0, 1.0, 1.0);
            gpu.gl().clear(glow::COLOR_BUFFER_BIT);
        }

        let fence = gpu.fence().expect("fence").expect("a fence fd");
        assert!(fence.as_raw_fd() >= 0);

        // A sync_file is pollable, and polling one that is already signalled
        // returns immediately. This is the same fd a drm_syncobj timeline
        // takes, so if it were not a real fence this would fail here rather
        // than somewhere deep in the compositor.
        let mut poll = libc::pollfd {
            fd: fence.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut poll, 1, 2000) };
        assert_eq!(ready, 1, "the fence never signalled");
        assert_ne!(poll.revents & libc::POLLIN, 0, "revents {}", poll.revents);
    }

    /// The question the whole rewrite turns on.
    ///
    /// One GPU renders; a second, entirely separate EGL display imports the
    /// same fd and reads the pixels back. If this passes, Servo can paint the
    /// shell into a buffer the compositor scans out without a copy — which is
    /// what WPE gave us and what Servo does not.
    #[test]
    fn a_buffer_rendered_on_one_context_is_readable_on_another() {
        let Some(painter) = gpu() else { return };
        let Some(compositor) = gpu() else { return };

        let buffer = painter.allocate(64, 64).expect("allocate");

        // Paint.
        painter.make_current().expect("make current");
        let target = painter.import(&buffer).expect("import on painter");
        let framebuffer = painter.framebuffer(&target).expect("framebuffer");
        framebuffer.bind();
        unsafe {
            painter.gl().viewport(0, 0, 64, 64);
            // Deliberately not grey: a channel mix-up between the two contexts
            // has to be visible in the result.
            painter.gl().clear_color(1.0, 0.5, 0.0, 1.0);
            painter.gl().clear(glow::COLOR_BUFFER_BIT);
            // Stands in for the fence that will replace it: without explicit
            // sync the reader has to be told the paint finished somehow.
            painter.gl().finish();
        }

        // Read back on the other GPU, through the fd alone.
        compositor.make_current().expect("make current");
        let imported = compositor.import(&buffer).expect("import on compositor");
        let readback = compositor.framebuffer(&imported).expect("framebuffer");
        readback.bind();

        let mut pixels = [0u8; 4];
        unsafe {
            compositor.gl().read_pixels(
                32,
                32,
                1,
                1,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(Some(&mut pixels)),
            );
        }

        // 8-bit rounding on the 0.5 channel, so allow a little slack.
        assert_eq!(pixels[0], 255, "red channel: {pixels:?}");
        assert!(
            (pixels[1] as i32 - 128).abs() <= 2,
            "green channel: {pixels:?}"
        );
        assert_eq!(pixels[2], 0, "blue channel: {pixels:?}");
        assert_eq!(pixels[3], 255, "alpha channel: {pixels:?}");
    }
}
