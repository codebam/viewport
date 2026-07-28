// SPDX-License-Identifier: MIT
//
// The `wl_buffer` wrappers: ImportDmaWl and ImportMemWl.
//
// These are thin. A dmabuf-backed `wl_buffer` unwraps to a `Dmabuf` and goes
// through `ImportDma` unchanged, so Smithay's default body already does the
// right thing. An shm one needs real work, because the shared memory a client
// wrote is described by a stride and an offset that need not match how Vulkan
// wants the rows laid out.
//
// Behind the `wayland` feature so the renderer stays usable — and testable —
// without a Wayland display.

use smithay::backend::renderer::{ImportDmaWl, ImportMem, ImportMemWl, Texture as _};
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::Resource;
use smithay::utils::{Buffer as BufferCoord, Rectangle};
use smithay::wayland::compositor::SurfaceData;
use smithay::wayland::shm::with_buffer_contents;

use crate::renderer::{Error, VulkanRenderer, VulkanTexture};

/// A dmabuf `wl_buffer` is just a `Dmabuf` underneath, and Smithay's default
/// body unwraps it and calls `import_dmabuf`. Nothing here to add.
impl ImportDmaWl for VulkanRenderer {}

/// Repack `src` into tightly packed rows.
///
/// A client's shm buffer has whatever stride it chose — often padded for
/// alignment — while the upload path wants `width * 4` per row. Returning a
/// borrow in the common case where they already agree keeps the copy out of
/// the path most clients take.
pub(crate) fn repack(
    src: &[u8],
    offset: usize,
    stride: usize,
    width: usize,
    height: usize,
) -> Result<std::borrow::Cow<'_, [u8]>, Error> {
    let packed = width * 4;
    let needed = offset
        .checked_add(stride.saturating_mul(height.saturating_sub(1)))
        .and_then(|v| v.checked_add(packed))
        .ok_or_else(|| Error::Unsupported("shm buffer dimensions overflow".to_owned()))?;
    if src.len() < needed {
        return Err(Error::Unsupported(format!(
            "shm buffer is {} bytes; {width}x{height} at stride {stride} needs {needed}",
            src.len()
        )));
    }

    if stride == packed && offset == 0 {
        return Ok(std::borrow::Cow::Borrowed(&src[..packed * height]));
    }

    let mut out = Vec::with_capacity(packed * height);
    for row in 0..height {
        let start = offset + row * stride;
        out.extend_from_slice(&src[start..start + packed]);
    }
    Ok(std::borrow::Cow::Owned(out))
}

impl ImportMemWl for VulkanRenderer {
    fn import_shm_buffer(
        &mut self,
        buffer: &WlBuffer,
        _surface: Option<&SurfaceData>,
        damage: &[Rectangle<i32, BufferCoord>],
    ) -> Result<Self::TextureId, Self::Error> {
        // The closure returns a Result so a bad buffer is reported rather than
        // panicking inside someone else's callback.
        let imported = with_buffer_contents(buffer, |pointer, len, data| {
            let fourcc = smithay::wayland::shm::shm_format_to_fourcc(data.format).ok_or_else(
                || Error::Unsupported(format!("no fourcc for shm format {:?}", data.format)),
            )?;

            let (width, height) = (data.width.max(0) as usize, data.height.max(0) as usize);
            if width == 0 || height == 0 {
                return Err(Error::Unsupported("a zero-sized shm buffer".to_owned()));
            }

            // SAFETY: with_buffer_contents guarantees the mapping is valid and
            // the pool is locked for the duration of this closure.
            let bytes = unsafe { std::slice::from_raw_parts(pointer, len) };
            let packed = repack(
                bytes,
                data.offset.max(0) as usize,
                data.stride.max(0) as usize,
                width,
                height,
            )?;

            Ok((fourcc, width as i32, height as i32, packed.into_owned()))
        })
        .map_err(|e| Error::Unsupported(format!("not an shm buffer: {e}")))??;

        let (fourcc, width, height, pixels) = imported;
        let id = buffer.id();

        // An existing texture of the same shape is updated in place. Creating
        // a new image per commit would mean an allocation and a full upload
        // every frame for every shm surface, which is the whole cost of the
        // shm path doubled.
        // Every other cached buffer has now missed this commit.
        for entry in self.shm.iter_mut().filter(|e| e.id != id) {
            if entry.pending.len() < MAX_PENDING {
                entry.pending.extend_from_slice(damage);
            }
        }

        let found = self
            .shm
            .iter_mut()
            .find(|e| {
                e.id == id
                    && e.texture.width() as i32 == width
                    && e.texture.height() as i32 == height
                    && e.texture.image().fourcc() == fourcc
            })
            .map(|e| (e.texture.clone(), std::mem::take(&mut e.pending)));

        if let Some((texture, pending)) = found {
            let whole = Rectangle::from_size((width, height).into());
            for region in plan_upload(damage, &pending, whole) {
                self.update_memory(&texture, &pixels, region)?;
            }
            return Ok(texture);
        }

        // New, or a different shape: a full upload either way.
        let texture = self.import_memory(&pixels, fourcc, (width, height).into(), false)?;
        self.shm.retain(|e| e.id != id);
        self.shm.push(ShmEntry {
            id,
            texture: texture.clone(),
            pending: Vec::new(),
        });
        Ok(texture)
    }
}

impl VulkanRenderer {
    /// Forget the texture cached for a `wl_buffer`.
    ///
    /// Called when the buffer is destroyed; without it the cache grows for the
    /// lifetime of the compositor.
    pub fn forget_shm_buffer(&mut self, buffer: &WlBuffer) {
        let id = buffer.id();
        self.shm.retain(|e| e.id != id);
    }
}

/// One cached shm texture.
pub(crate) struct ShmEntry {
    pub id: smithay::reexports::wayland_server::backend::ObjectId,
    pub texture: VulkanTexture,
    /// Damage that landed while some other buffer was the one being drawn.
    ///
    /// Smithay reports damage since this renderer last imported *for the
    /// surface*, and its own model is one texture per surface. A texture per
    /// `wl_buffer` is faster — a client that alternates two buffers uploads
    /// only what changed instead of everything — but it means a buffer coming
    /// back around has missed every commit made through the other one. Those
    /// regions are collected here and uploaded when it does.
    ///
    /// Without it the untouched parts of the returning buffer are two commits
    /// old: a terminal shows glyphs from an older frame in the cells that were
    /// not rewritten, and alternates as the buffers do.
    pub pending: Vec<Rectangle<i32, BufferCoord>>,
}

/// The cache entry type, kept here so `renderer.rs` does not need the Wayland
/// types when the feature is off.
pub(crate) type ShmCache = Vec<ShmEntry>;

/// Past this many outstanding regions a full upload is cheaper than tracking
/// them, and much cheaper than getting the bookkeeping wrong.
const MAX_PENDING: usize = 32;

/// What to upload into a returning buffer: this commit's damage, and every
/// commit it missed while another buffer was in use.
///
/// An empty result means nothing changed and the upload can be skipped, which
/// the trait allows. A `pending` list at the limit means take the whole
/// buffer instead of replaying a long history.
fn plan_upload(
    damage: &[Rectangle<i32, BufferCoord>],
    pending: &[Rectangle<i32, BufferCoord>],
    whole: Rectangle<i32, BufferCoord>,
) -> Vec<Rectangle<i32, BufferCoord>> {
    if pending.len() >= MAX_PENDING {
        return vec![whole];
    }
    damage.iter().chain(pending.iter()).copied().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, BufferCoord> {
        Rectangle::new((x, y).into(), (w, h).into())
    }

    #[test]
    fn a_returning_buffer_is_given_the_commits_it_missed() {
        // A client alternating two buffers: this one was last drawn two
        // commits ago, so the damage from the commit in between is its to
        // catch up on. Uploading only the newest damage leaves the rest of it
        // a generation old — in a terminal, glyphs from an older frame in
        // every cell that was not rewritten.
        let whole = rect(0, 0, 100, 100);
        let now = [rect(0, 0, 10, 10)];
        let missed = [rect(50, 50, 10, 10)];
        let plan = plan_upload(&now, &missed, whole);
        assert_eq!(plan, vec![rect(0, 0, 10, 10), rect(50, 50, 10, 10)]);
    }

    #[test]
    fn nothing_to_do_stays_nothing_to_do() {
        // No damage and nothing missed: the client says the buffer is
        // unchanged and the upload is skipped entirely.
        assert!(plan_upload(&[], &[], rect(0, 0, 100, 100)).is_empty());
    }

    #[test]
    fn a_long_history_is_replaced_by_the_whole_buffer() {
        let whole = rect(0, 0, 100, 100);
        let missed: Vec<_> = (0..MAX_PENDING as i32).map(|i| rect(i, 0, 1, 1)).collect();
        assert_eq!(plan_upload(&[rect(0, 0, 5, 5)], &missed, whole), vec![whole]);
    }

    #[test]
    fn a_tightly_packed_buffer_is_not_copied() {
        // 2x2, stride exactly the row length.
        let src: Vec<u8> = (0..16).collect();
        let out = repack(&src, 0, 8, 2, 2).expect("repack");
        assert!(matches!(out, std::borrow::Cow::Borrowed(_)), "needlessly copied");
        assert_eq!(&*out, &src[..]);
    }

    #[test]
    fn a_padded_stride_is_repacked() {
        // 2x2 with 4 bytes of padding per row.
        let mut src = Vec::new();
        src.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        src.extend_from_slice(&[0, 0, 0, 0]);
        src.extend_from_slice(&[9, 10, 11, 12, 13, 14, 15, 16]);
        src.extend_from_slice(&[0, 0, 0, 0]);

        let out = repack(&src, 0, 12, 2, 2).expect("repack");
        assert!(matches!(out, std::borrow::Cow::Owned(_)), "should have copied");
        assert_eq!(
            &*out,
            &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]
        );
    }

    #[test]
    fn an_offset_is_honoured() {
        let mut src = vec![0xff; 4];
        src.extend_from_slice(&[1, 2, 3, 4]);
        let out = repack(&src, 4, 4, 1, 1).expect("repack");
        assert_eq!(&*out, &[1, 2, 3, 4]);
    }

    #[test]
    fn a_short_buffer_is_an_error_rather_than_a_read_past_the_end() {
        // The last row is missing.
        let src = vec![0u8; 8];
        let error = repack(&src, 0, 8, 2, 2).expect_err("a short buffer must be refused");
        assert!(error.to_string().contains("needs"), "{error}");
    }

    #[test]
    fn absurd_dimensions_do_not_overflow() {
        let src = vec![0u8; 16];
        let error = repack(&src, 0, usize::MAX, 2, 2).expect_err("must not overflow");
        assert!(
            error.to_string().contains("overflow") || error.to_string().contains("needs"),
            "{error}"
        );
    }
}
