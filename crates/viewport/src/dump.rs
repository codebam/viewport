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
use smithay::backend::renderer::{
    Bind, Color32F, ExportMem, Frame, Offscreen, Renderer, TextureMapping,
};
use smithay::utils::{Buffer as BufferCoord, Physical, Point, Rectangle, Size, Transform};

use viewport_vulkan::{VulkanRenderer, VulkanTexture};

/// Where to write, if anywhere.
pub fn target() -> Option<std::path::PathBuf> {
    std::env::var_os("VIEWPORT_DUMP_SHELL").map(std::path::PathBuf::from)
}

/// Draw `texture` on its own and write the result as a binary PPM.
///
/// Deliberately not a copy of the texture: an imported dma-buf is created
/// without TRANSFER_SRC, because asking for it can make a modifier that would
/// otherwise work be refused, so it usually cannot be read back directly.
/// Drawing it into an offscreen we allocated is the same thing an output does
/// and is always available.
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
            .clear(Color32F::from([1.0, 0.0, 1.0, 1.0]), &[Rectangle::from_size(size)])
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
