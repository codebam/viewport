// SPDX-License-Identifier: GPL-3.0-or-later
//
// Writing the shell's own frame to a file, for when the screen is the only
// witness.
//
// "The right monitor is grey" has two very different causes that produce the
// same photograph: WebKit painted nothing into that half of the buffer, or it
// painted and the compositor put the wrong part of it on screen. Nothing in
// the log distinguishes them, because both draw one element at the right
// offset. Reading the buffer back does.
//
// Off unless VIEWPORT_DUMP_SHELL names a path.

use anyhow::{Context, Result};

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::{Bind, Color32F, ExportMem, Frame, Offscreen, Renderer};
use smithay::utils::{Buffer as BufferCoord, Physical, Point, Rectangle, Size, Transform};

use viewport_vulkan::{VulkanRenderer, VulkanTexture};

/// Where to write the shell's own buffer, if anywhere.
// Only the web engine's paths use this; dead, honestly, without it.
#[cfg_attr(not(feature = "wpe"), allow(dead_code))]
pub fn target() -> Option<std::path::PathBuf> {
    std::env::var_os("VIEWPORT_DUMP_SHELL").map(std::path::PathBuf::from)
}

/// Where to write composited outputs, if anywhere.
///
/// The shell's buffer alone cannot answer "why is there grey where the bar
/// should be": that is a question about what survived compositing, and the
/// only place it exists is the output.
pub fn output_target() -> Option<std::path::PathBuf> {
    std::env::var_os("VIEWPORT_DUMP_OUTPUT").map(std::path::PathBuf::from)
}

/// Composite `elements` for an output of `size` and write the result.
///
/// The same list the output was given, drawn the same way, so what lands here
/// is what landed on screen — including anything an occlusion decision left
/// undrawn, which is the whole reason for looking.
pub fn output_frame<R, B, E>(
    renderer: &mut R,
    elements: &[E],
    size: Size<i32, Physical>,
    clear: [f32; 4],
    path: &std::path::Path,
) -> Result<()>
where
    // The buffer an offscreen is: a dmabuf for the Vulkan renderer, a
    // renderbuffer for GLES. Which one is the backend's business.
    R: Renderer + Bind<B> + Offscreen<B> + ExportMem,
    E: smithay::backend::renderer::element::RenderElement<R>,
    <R as smithay::backend::renderer::RendererSuper>::Error: Send + Sync + 'static,
{
    use smithay::backend::renderer::damage::OutputDamageTracker;
    use smithay::backend::renderer::Texture as _;

    let buffer_size: Size<i32, BufferCoord> = (size.w, size.h).into();
    let mut target = renderer
        .create_buffer(Fourcc::Argb8888, buffer_size)
        .context("allocating the dump target")?;

    let mut tracker = OutputDamageTracker::new(size, 1.0, Transform::Normal);
    {
        let mut framebuffer = renderer.bind(&mut target).context("binding it")?;
        tracker
            .render_output(
                renderer,
                &mut framebuffer,
                0,
                elements,
                Color32F::from(clear),
            )
            .map_err(|e| anyhow::anyhow!("compositing the dump: {e:?}"))?;

        let mapping = renderer
            .copy_framebuffer(
                &framebuffer,
                Rectangle::from_size(buffer_size),
                Fourcc::Argb8888,
            )
            .context("reading it back")?;
        let (w, h) = (mapping.width(), mapping.height());
        let pixels = renderer.map_texture(&mapping).context("mapping it")?;
        write_ppm(path, w, h, pixels)?;
    }

    tracing::info!("wrote a composited output to {}", path.display());
    Ok(())
}

/// Copy `texture` into an image the compositor owns.
///
/// The shell's buffer belongs to WebKit, and WebKit will not paint the next
/// frame until it has been given back — so it cannot be both handed back and
/// sampled from. Dup'ing the buffer's fds does not help: a dup'd fd is the
/// same memory, so the engine would be painting into the picture on screen.
///
/// A full-screen copy per shell frame is the price. The shell paints when the
/// desktop changes rather than continuously, so it is not a per-frame cost.
// Only the web engine's paths use this; dead, honestly, without it.
#[cfg_attr(not(feature = "wpe"), allow(dead_code))]
///
/// Generic over the renderer, because the shell's copy runs on whichever one
/// owns the device — Vulkan where a Vulkan device does, OpenGL in a virtual
/// machine where none does. The body is written once and compiled twice, which
/// is what keeps the two honest.
pub fn copy_texture<R>(
    renderer: &mut R,
    texture: &R::TextureId,
    into: &mut smithay::backend::allocator::dmabuf::Dmabuf,
    size: Size<i32, Physical>,
) -> Result<()>
where
    R: smithay::backend::renderer::Renderer
        + smithay::backend::renderer::Bind<smithay::backend::allocator::dmabuf::Dmabuf>,
    <R as smithay::backend::renderer::RendererSuper>::Error: Send + Sync + 'static,
{
    let mut framebuffer = renderer.bind(into).context("binding the shell copy")?;
    let mut frame = renderer
        .render(&mut framebuffer, size, Transform::Normal)
        .context("starting the copy")?;
    frame
        .render_texture_at(
            texture,
            Point::from((0, 0)),
            1,
            1.0,
            Transform::Normal,
            &[Rectangle::from_size(size)],
            &[],
            1.0,
        )
        .context("copying the shell")?;
    let _ = frame.finish().context("finishing the copy")?;
    Ok(())
}

/// Allocate an image the compositor owns, for [`copy_texture`].
// Only the web engine's paths use this; dead, honestly, without it.
#[cfg_attr(not(feature = "wpe"), allow(dead_code))]
///
/// From the GBM allocator rather than from the renderer: OpenGL cannot hand out
/// a DMA-BUF, and asking the renderer for one is what tied this path to Vulkan
/// and so to whatever Vulkan device happened to load.
///
/// `modifiers` are the ones the renderer that will import this buffer
/// advertises for ARGB8888. Allocating implicitly instead is what made every
/// copy fail on a machine whose shell renderer is Vulkan:
///
///   binding the shell copy: Intel(R) UHD Graphics 620 (KBL GT2) does not
///   support DrmFourcc(AR24) with modifier 0xffffffffffffff
///
/// `VK_EXT_image_drm_format_modifier` enumerates explicit modifiers and never
/// `DRM_FORMAT_MOD_INVALID`, so a Vulkan renderer cannot import an implicitly
/// allocated buffer at all — not on this GPU, not on any. OpenGL imports
/// either kind, which is why the implicit allocation looked correct for as
/// long as the copy happened to run on GLES.
pub fn owned_image(
    allocator: &mut smithay::backend::allocator::gbm::GbmAllocator<
        smithay::backend::drm::DrmDeviceFd,
    >,
    size: Size<i32, Physical>,
    modifiers: &[smithay::backend::allocator::Modifier],
) -> Result<smithay::backend::allocator::dmabuf::Dmabuf> {
    use smithay::backend::allocator::dmabuf::AsDmabuf as _;
    use smithay::backend::allocator::{Allocator as _, Modifier};
    // Implicit only when there is nothing else to ask for: a renderer that
    // advertises no modifier for this format is an OpenGL one on a driver
    // without the modifier extensions, and that one takes an implicit buffer.
    let modifiers = if modifiers.is_empty() {
        &[Modifier::Invalid][..]
    } else {
        modifiers
    };
    let buffer = allocator
        .create_buffer(size.w as u32, size.h as u32, Fourcc::Argb8888, modifiers)
        .context("allocating an image for the shell")?;
    buffer
        .export()
        .context("exporting the shell's image as a dma-buf")
}

/// Draw `texture` on its own and write the result as a binary PPM.
///
/// Deliberately not a copy of the texture: an imported dma-buf is created
/// without TRANSFER_SRC, because asking for it can make a modifier that would
/// otherwise work be refused, so it usually cannot be read back directly.
/// Drawing it into an offscreen we allocated is the same thing an output does
/// and is always available.
// Only the web engine's paths use this; dead, honestly, without it.
#[cfg_attr(not(feature = "wpe"), allow(dead_code))]
pub fn shell_frame(
    renderer: &mut VulkanRenderer,
    texture: &VulkanTexture,
    path: &std::path::Path,
) -> Result<()> {
    use smithay::backend::renderer::Texture as _;

    let size: Size<i32, Physical> = (texture.width() as i32, texture.height() as i32).into();

    let buffer_size: Size<i32, BufferCoord> = (size.w, size.h).into();
    let mut target: smithay::backend::allocator::dmabuf::Dmabuf = renderer
        .create_buffer(Fourcc::Argb8888, buffer_size)
        .context("allocating the dump target")?;

    {
        let mut framebuffer = renderer.bind(&mut target).context("binding it")?;
        let mut frame = renderer
            .render(&mut framebuffer, size, Transform::Normal)
            .context("starting the dump frame")?;
        // Magenta, so "WebKit painted nothing here" and "the shell painted
        // black here" are not the same picture in the dump either.
        frame
            .clear(
                Color32F::from([1.0, 0.0, 1.0, 1.0]),
                &[Rectangle::from_size(size)],
            )
            .context("clearing")?;
        frame
            .render_texture_at(
                texture,
                Point::from((0, 0)),
                1,
                1.0,
                Transform::Normal,
                &[Rectangle::from_size(size)],
                &[],
                1.0,
            )
            .context("drawing the shell")?;
        let _ = frame.finish().context("finishing")?;

        let mapping = renderer
            .copy_framebuffer(
                &framebuffer,
                Rectangle::from_size(buffer_size),
                Fourcc::Argb8888,
            )
            .context("reading it back")?;
        let pixels = renderer.map_texture(&mapping).context("mapping it")?;
        write_ppm(path, mapping.width(), mapping.height(), pixels)?;
    }

    tracing::info!("wrote the shell's frame to {}", path.display());
    Ok(())
}

/// Binary PPM: three bytes a pixel and a nine-byte header, which every image
/// viewer reads and which needs no encoder.
fn write_ppm(path: &std::path::Path, width: u32, height: u32, pixels: &[u8]) -> Result<()> {
    use std::io::Write as _;

    let mut out = Vec::with_capacity(pixels.len() / 4 * 3 + 32);
    out.extend_from_slice(format!("P6\n{width} {height}\n255\n").as_bytes());
    // ARGB8888 is named from the least significant byte up, so in memory the
    // order is B, G, R, A.
    for chunk in pixels.chunks_exact(4) {
        out.extend_from_slice(&[chunk[2], chunk[1], chunk[0]]);
    }

    std::fs::File::create(path)
        .with_context(|| format!("creating {}", path.display()))?
        .write_all(&out)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}
