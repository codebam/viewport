// SPDX-License-Identifier: MIT
//
// DRM formats, Vulkan formats, and the modifiers that connect them.
//
// A DMA-BUF describes its contents with a DRM fourcc and a modifier; a Vulkan
// image describes them with a `vk::Format` and a tiling. Importing one as the
// other means agreeing on both, and the modifier is the part that actually
// matters: two images with the same fourcc and different modifiers have
// entirely different memory layouts, and reading one as the other produces
// garbage rather than an error.

use ash::vk;
use smithay::backend::allocator::{Format, Fourcc, Modifier};
use smithay::backend::vulkan::PhysicalDevice;

/// The Vulkan format matching a DRM fourcc.
///
/// DRM fourccs name channels from the least significant byte up, so
/// `ARGB8888` is B, G, R, A in memory and maps to `B8G8R8A8_UNORM`. Getting
/// this backwards swaps red and blue, which is the classic symptom.
pub fn to_vulkan(fourcc: Fourcc) -> Option<vk::Format> {
    Some(match fourcc {
        Fourcc::Argb8888 | Fourcc::Xrgb8888 => vk::Format::B8G8R8A8_UNORM,
        Fourcc::Abgr8888 | Fourcc::Xbgr8888 => vk::Format::R8G8B8A8_UNORM,

        // Ten bits per channel, which is where HDR starts.
        Fourcc::Abgr2101010 | Fourcc::Xbgr2101010 => vk::Format::A2B10G10R10_UNORM_PACK32,
        Fourcc::Argb2101010 | Fourcc::Xrgb2101010 => vk::Format::A2R10G10B10_UNORM_PACK32,

        // Half float, for scRGB and anything with values outside 0..1.
        Fourcc::Abgr16161616f | Fourcc::Xbgr16161616f => vk::Format::R16G16B16A16_SFLOAT,

        Fourcc::Rgb565 => vk::Format::R5G6B5_UNORM_PACK16,
        Fourcc::R8 => vk::Format::R8_UNORM,
        Fourcc::Gr88 => vk::Format::R8G8_UNORM,

        // Multi-planar YUV, which is what a hardware video decoder produces.
        // Sampling one needs a `VkSamplerYcbcrConversion`; without that path
        // these must not be mapped, because a shader reading the luma plane as
        // if it were RGB gets a greyscale image and no error.
        //
        // NV21 and YVU420 are their neighbours with the two chroma channels
        // the other way round. Vulkan has no separate format for either: the
        // conversion's component swizzle puts them right, which is what
        // `chroma_swizzle` is for.
        Fourcc::Nv12 | Fourcc::Nv21 => vk::Format::G8_B8R8_2PLANE_420_UNORM,
        Fourcc::Nv16 => vk::Format::G8_B8R8_2PLANE_422_UNORM,
        Fourcc::Yuv420 | Fourcc::Yvu420 => vk::Format::G8_B8_R8_3PLANE_420_UNORM,
        Fourcc::Yuv422 => vk::Format::G8_B8_R8_3PLANE_422_UNORM,
        Fourcc::Yuv444 => vk::Format::G8_B8_R8_3PLANE_444_UNORM,

        // Ten and sixteen bit video. P010 carries its ten bits in the top of a
        // sixteen-bit word, which is exactly what Vulkan's `10X6` formats
        // describe — the `X6` is the six unused low bits.
        Fourcc::P010 => vk::Format::G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16,
        Fourcc::P016 => vk::Format::G16_B16R16_2PLANE_420_UNORM,

        _ => return None,
    })
}

/// Whether sampling this format needs a YCbCr conversion.
///
/// Not a property of the plane count: a compressed RGB modifier has auxiliary
/// planes and is still sampled as ordinary colour. What decides it is the
/// format being colour-difference encoded, which is what the sampler has to
/// undo before a shader sees anything meaningful.
pub fn is_yuv(fourcc: Fourcc) -> bool {
    matches!(
        fourcc,
        Fourcc::Nv12
            | Fourcc::Nv21
            | Fourcc::Nv16
            | Fourcc::Yuv420
            | Fourcc::Yvu420
            | Fourcc::Yuv422
            | Fourcc::Yuv444
            | Fourcc::P010
            | Fourcc::P016
    )
}

/// The component swizzle a YUV format needs on top of its Vulkan format.
///
/// NV21 and YVU420 hold Cr where their siblings hold Cb. Vulkan assigns the
/// chroma planes to fixed components — B is Cb, R is Cr — so the only way to
/// describe the swap is to exchange those two components in the conversion.
/// Getting it wrong is not subtle: faces come out blue.
pub fn chroma_swizzle(fourcc: Fourcc) -> vk::ComponentMapping {
    match fourcc {
        Fourcc::Nv21 | Fourcc::Yvu420 => vk::ComponentMapping {
            r: vk::ComponentSwizzle::B,
            g: vk::ComponentSwizzle::IDENTITY,
            b: vk::ComponentSwizzle::R,
            a: vk::ComponentSwizzle::IDENTITY,
        },
        _ => vk::ComponentMapping::default(),
    }
}

/// Whether a fourcc has an alpha channel.
///
/// `Xrgb8888` and `Argb8888` are the same Vulkan format, so this is the only
/// thing that distinguishes them — and it decides whether a surface is
/// composited with blending or drawn opaque.
pub fn has_alpha(fourcc: Fourcc) -> bool {
    matches!(
        fourcc,
        Fourcc::Argb8888
            | Fourcc::Abgr8888
            | Fourcc::Abgr2101010
            | Fourcc::Argb2101010
            | Fourcc::Abgr16161616f
    )
}

/// What a format can be used for on this device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModifierSupport {
    pub modifier: Modifier,
    /// Planes in the layout. A multi-planar modifier needs one fd per plane.
    pub planes: u32,
    /// Can be sampled from — an imported client buffer needs this.
    pub sampling: bool,
    /// Can be rendered into — an output buffer needs this.
    pub rendering: bool,
    /// Can be the source of a copy, which is what read-back needs.
    pub transfer_src: bool,
    /// Can be the destination of a copy, which is what a blit needs.
    pub transfer_dst: bool,
    /// Chroma siting the device can reconstruct. A YUV modifier that offers
    /// neither cannot be sampled through a conversion at all, whatever
    /// `sampling` says — `SAMPLED_IMAGE` on a multi-planar format only means
    /// the planes can be read, not that they can be assembled.
    pub cosited_chroma: bool,
    pub midpoint_chroma: bool,
    /// Chroma can be filtered linearly. Where it cannot, the luma filter has
    /// to drop to nearest with it: Vulkan requires the two to match unless the
    /// format says otherwise, and asking for a mismatch is invalid usage.
    pub linear_chroma: bool,
    /// The planes may live in separate allocations. Needed when an exporter
    /// hands over one fd per plane rather than one buffer with offsets.
    pub disjoint: bool,
}

impl ModifierSupport {
    /// Whether a YUV buffer with this modifier can actually be sampled.
    pub fn ycbcr_sampling(&self) -> bool {
        self.sampling && (self.cosited_chroma || self.midpoint_chroma)
    }
}

/// Every modifier this device supports for `fourcc`.
///
/// Returns an empty list for a format the device does not know, or where
/// `VK_EXT_image_drm_format_modifier` is missing — in both cases there is
/// nothing importable, which is what the caller needs to know.
pub fn modifiers(physical: &PhysicalDevice, fourcc: Fourcc) -> Vec<ModifierSupport> {
    let Some(format) = to_vulkan(fourcc) else {
        return Vec::new();
    };
    let Ok(properties) = physical.get_format_modifier_properties(format) else {
        return Vec::new();
    };

    properties
        .into_iter()
        .map(|property| ModifierSupport {
            modifier: Modifier::from(property.drm_format_modifier),
            planes: property.drm_format_modifier_plane_count,
            sampling: property
                .drm_format_modifier_tiling_features
                .contains(vk::FormatFeatureFlags::SAMPLED_IMAGE),
            rendering: property
                .drm_format_modifier_tiling_features
                .contains(vk::FormatFeatureFlags::COLOR_ATTACHMENT),
            // Asked about rather than assumed. Requesting a usage the
            // modifier does not support makes vkCreateImage refuse a buffer
            // that would otherwise have imported fine, and the failure looks
            // like an unrelated format problem.
            transfer_src: property
                .drm_format_modifier_tiling_features
                .contains(vk::FormatFeatureFlags::TRANSFER_SRC),
            transfer_dst: property
                .drm_format_modifier_tiling_features
                .contains(vk::FormatFeatureFlags::TRANSFER_DST),
            cosited_chroma: property
                .drm_format_modifier_tiling_features
                .contains(vk::FormatFeatureFlags::COSITED_CHROMA_SAMPLES),
            midpoint_chroma: property
                .drm_format_modifier_tiling_features
                .contains(vk::FormatFeatureFlags::MIDPOINT_CHROMA_SAMPLES),
            linear_chroma: property
                .drm_format_modifier_tiling_features
                .contains(vk::FormatFeatureFlags::SAMPLED_IMAGE_YCBCR_CONVERSION_LINEAR_FILTER),
            disjoint: property
                .drm_format_modifier_tiling_features
                .contains(vk::FormatFeatureFlags::DISJOINT),
        })
        .collect()
}

/// The formats a client may advertise, as a Smithay format set.
///
/// Only sampleable combinations: a compositor advertising a modifier it cannot
/// actually sample from produces buffers it has to reject later, which the
/// client experiences as a black window rather than a negotiation failure.
///
/// Colour formats are held to a single plane. A multi-planar RGB modifier is
/// a compression scheme with an auxiliary plane, and this renderer's import
/// path does not understand the pairing. YUV is the opposite case: multiple
/// planes are the format, and the extra requirement there is that the device
/// can assemble them, which `ycbcr_sampling` is.
pub fn importable(physical: &PhysicalDevice, formats: &[Fourcc]) -> Vec<Format> {
    formats
        .iter()
        .flat_map(|&code| {
            let yuv = is_yuv(code);
            modifiers(physical, code)
                .into_iter()
                .filter(move |support| {
                    if yuv {
                        support.ycbcr_sampling()
                    } else {
                        support.sampling && support.planes == 1
                    }
                })
                .map(move |support| Format {
                    code,
                    modifier: support.modifier,
                })
        })
        .collect()
}

/// The formats worth advertising to clients, in preference order.
///
/// The YUV entries are what a hardware video decoder hands out. Without them
/// a player using VA-API or NVDEC has to convert every frame on the CPU before
/// it can hand the compositor something importable, which is the single
/// largest avoidable cost in playing a video.
pub const COMMON_FORMATS: &[Fourcc] = &[
    Fourcc::Argb8888,
    Fourcc::Xrgb8888,
    Fourcc::Abgr8888,
    Fourcc::Xbgr8888,
    Fourcc::Abgr2101010,
    Fourcc::Xbgr2101010,
    Fourcc::Abgr16161616f,
    Fourcc::Nv12,
    Fourcc::Nv21,
    Fourcc::Nv16,
    Fourcc::P010,
    Fourcc::P016,
    Fourcc::Yuv420,
    Fourcc::Yvu420,
    Fourcc::Yuv422,
    Fourcc::Yuv444,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argb_maps_to_bgra_not_rgba() {
        // The mapping everyone gets wrong. DRM names channels from the least
        // significant byte, Vulkan names them in memory order, so the two read
        // in opposite directions.
        assert_eq!(
            to_vulkan(Fourcc::Argb8888),
            Some(vk::Format::B8G8R8A8_UNORM)
        );
        assert_eq!(
            to_vulkan(Fourcc::Abgr8888),
            Some(vk::Format::R8G8B8A8_UNORM)
        );
    }

    #[test]
    fn opaque_and_alpha_variants_share_a_vulkan_format() {
        // Which is why has_alpha exists: the Vulkan format cannot tell them
        // apart, and blending depends on the difference.
        assert_eq!(to_vulkan(Fourcc::Xrgb8888), to_vulkan(Fourcc::Argb8888));
        assert!(has_alpha(Fourcc::Argb8888));
        assert!(!has_alpha(Fourcc::Xrgb8888));
    }

    #[test]
    fn ten_bit_formats_are_mapped() {
        // HDR needs more than eight bits per channel, so these must not be
        // silently unsupported.
        assert_eq!(
            to_vulkan(Fourcc::Abgr2101010),
            Some(vk::Format::A2B10G10R10_UNORM_PACK32)
        );
        assert_eq!(
            to_vulkan(Fourcc::Abgr16161616f),
            Some(vk::Format::R16G16B16A16_SFLOAT)
        );
        assert!(COMMON_FORMATS.contains(&Fourcc::Abgr2101010));
    }

    #[test]
    fn an_unknown_fourcc_is_none_rather_than_a_guess() {
        // Packed YUV, which has no multi-planar Vulkan format and so is not
        // mapped — unlike the planar ones below.
        assert_eq!(to_vulkan(Fourcc::Yuyv), None);
        assert!(!has_alpha(Fourcc::Yuyv));
        assert!(!is_yuv(Fourcc::Yuyv));
    }

    #[test]
    fn the_video_formats_map_to_their_multi_planar_equivalents() {
        // What a hardware decoder hands over. Mapping one to a single-plane
        // format would sample the luma plane as if it were colour: a greyscale
        // picture, with nothing anywhere reporting an error.
        assert_eq!(
            to_vulkan(Fourcc::Nv12),
            Some(vk::Format::G8_B8R8_2PLANE_420_UNORM)
        );
        assert_eq!(
            to_vulkan(Fourcc::Yuv420),
            Some(vk::Format::G8_B8_R8_3PLANE_420_UNORM)
        );
        assert!(COMMON_FORMATS.contains(&Fourcc::Nv12));
        assert!(COMMON_FORMATS.contains(&Fourcc::P010));
    }

    #[test]
    fn p010_keeps_its_ten_bits_in_the_top_of_a_sixteen_bit_word() {
        // The `10X6` in the Vulkan name is the six unused low bits, which is
        // exactly how P010 is laid out. Mapping it to a plain 16-bit format
        // would read the padding as picture and come out a stop too dark.
        assert_eq!(
            to_vulkan(Fourcc::P010),
            Some(vk::Format::G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16)
        );
        assert_eq!(
            to_vulkan(Fourcc::P016),
            Some(vk::Format::G16_B16R16_2PLANE_420_UNORM)
        );
    }

    #[test]
    fn the_chroma_swapped_formats_share_a_vulkan_format_and_differ_by_swizzle() {
        // Vulkan has no NV21. It is NV12 with Cb and Cr the other way round,
        // and the only place that can be said is the conversion's swizzle —
        // get it wrong and faces come out blue.
        assert_eq!(to_vulkan(Fourcc::Nv21), to_vulkan(Fourcc::Nv12));
        assert_eq!(to_vulkan(Fourcc::Yvu420), to_vulkan(Fourcc::Yuv420));

        let swapped = chroma_swizzle(Fourcc::Nv21);
        assert_eq!(swapped.r, vk::ComponentSwizzle::B);
        assert_eq!(swapped.b, vk::ComponentSwizzle::R);
        let plain = chroma_swizzle(Fourcc::Nv12);
        assert_eq!(plain.r, vk::ComponentSwizzle::IDENTITY);
        assert_eq!(plain.b, vk::ComponentSwizzle::IDENTITY);
    }

    #[test]
    fn yuv_has_no_alpha_and_is_recognised_as_yuv() {
        // Nothing in the YUV set carries alpha, so every one of them composites
        // opaque. `has_alpha` returning true here would blend a video against
        // whatever is behind it using undefined bytes.
        for code in COMMON_FORMATS.iter().copied().filter(|&c| is_yuv(c)) {
            assert!(!has_alpha(code), "{code:?} must not claim alpha");
            assert!(to_vulkan(code).is_some(), "{code:?} must map");
        }
        assert!(is_yuv(Fourcc::Nv12));
        assert!(!is_yuv(Fourcc::Argb8888));
    }

    #[test]
    fn ycbcr_sampling_needs_more_than_a_sampleable_modifier() {
        // SAMPLED_IMAGE on a multi-planar format only says the planes can be
        // read. Without a chroma siting the device can reconstruct, they
        // cannot be assembled into colour at all.
        let mut support = ModifierSupport {
            modifier: Modifier::Linear,
            planes: 2,
            sampling: true,
            rendering: false,
            transfer_src: false,
            transfer_dst: false,
            cosited_chroma: false,
            midpoint_chroma: false,
            linear_chroma: false,
            disjoint: false,
        };
        assert!(!support.ycbcr_sampling());
        support.midpoint_chroma = true;
        assert!(support.ycbcr_sampling());
        support.sampling = false;
        assert!(!support.ycbcr_sampling());
    }
}
