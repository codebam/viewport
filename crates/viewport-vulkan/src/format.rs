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

        _ => return None,
    })
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
        })
        .collect()
}

/// The formats a client may advertise, as a Smithay format set.
///
/// Only single-plane, sampleable combinations: a compositor advertising a
/// modifier it cannot actually sample from produces buffers it has to reject
/// later, which the client experiences as a black window rather than a
/// negotiation failure.
pub fn importable(physical: &PhysicalDevice, formats: &[Fourcc]) -> Vec<Format> {
    formats
        .iter()
        .flat_map(|&code| {
            modifiers(physical, code)
                .into_iter()
                .filter(|support| support.sampling && support.planes == 1)
                .map(move |support| Format {
                    code,
                    modifier: support.modifier,
                })
        })
        .collect()
}

/// The formats worth advertising to clients, in preference order.
pub const COMMON_FORMATS: &[Fourcc] = &[
    Fourcc::Argb8888,
    Fourcc::Xrgb8888,
    Fourcc::Abgr8888,
    Fourcc::Xbgr8888,
    Fourcc::Abgr2101010,
    Fourcc::Xbgr2101010,
    Fourcc::Abgr16161616f,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argb_maps_to_bgra_not_rgba() {
        // The mapping everyone gets wrong. DRM names channels from the least
        // significant byte, Vulkan names them in memory order, so the two read
        // in opposite directions.
        assert_eq!(to_vulkan(Fourcc::Argb8888), Some(vk::Format::B8G8R8A8_UNORM));
        assert_eq!(to_vulkan(Fourcc::Abgr8888), Some(vk::Format::R8G8B8A8_UNORM));
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
        assert_eq!(to_vulkan(Fourcc::Yuyv), None);
        assert!(!has_alpha(Fourcc::Yuyv));
    }
}
