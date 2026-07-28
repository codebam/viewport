// SPDX-License-Identifier: GPL-3.0-or-later
//
// High dynamic range. Ports src/hdr.c.
//
// Two halves, and both are needed before anything looks right.
//
// The output half is switching the monitor into an HDR mode: BT.2020 primaries
// and the PQ transfer function, which is what the display expects when it
// lights its panel past the SDR range. On its own that makes everything look
// worse — every SDR window is suddenly interpreted as if it were HDR, which
// washes it out.
//
// The client half is colour management: a client says what its content
// actually is, through wp-color-management-v1, and the renderer converts it
// into whatever the output is in. That half is already here, in
// `color_management.rs` and `viewport-vulkan`'s colour pipeline, and it is
// what makes the compositor convert rather than reinterpret.
//
// Smithay's DRM backend has no notion of either connector property, so the
// mode switch is two properties set by hand: `Colorspace`, and a blob holding
// the `hdr_output_metadata` structure the kernel defines. Both go in one
// atomic commit, tested before it is kept — a display that refuses must be
// left showing what it was showing rather than going dark while someone works
// out why.

use smithay::reexports::drm::control::{connector, property, Device as ControlDevice};

/// `struct hdr_output_metadata` as the kernel reads it, with the
/// `hdr_metadata_infoframe` it contains. From `include/uapi/drm/drm_mode.h`.
///
/// Laid out by hand rather than derived: the kernel reads these bytes
/// directly, so the field order and the padding are the interface.
/// A chromaticity coordinate, in units of 0.00002.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Coordinate {
    x: u16,
    y: u16,
}

/// `struct hdr_metadata_infoframe`.
///
/// The primaries are x and y interleaved per colour, not three x values
/// followed by three y values. Both layouts are the same number of bytes, so
/// getting it wrong costs nothing until someone fills them in and the display
/// is handed a green primary where it expected a red one.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct HdrMetadataInfoframe {
    /// Electro-optical transfer function. 2 is SMPTE ST 2084, which is PQ.
    eotf: u8,
    /// Zero: static metadata, type 1.
    metadata_type: u8,

    /// The mastering display's primaries and white point. Left at zero, which
    /// means unset — the display's own capabilities are then used rather than
    /// numbers this compositor would be inventing.
    display_primaries: [Coordinate; 3],
    white_point: Coordinate,
    max_display_mastering_luminance: u16,
    min_display_mastering_luminance: u16,
    max_cll: u16,
    max_fall: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct HdrOutputMetadata {
    /// Zero: the only type the kernel defines.
    metadata_type: u32,
    /// A union in the kernel, with one member.
    hdmi_metadata_type1: HdrMetadataInfoframe,
}

/// SMPTE ST 2084, the PQ curve.
const EOTF_ST2084: u8 = 2;

/// The metadata the display is handed when HDR is on, as the kernel reads it.
///
/// Written out byte by byte rather than by pointing at the struct above. The
/// struct has two bytes of trailing padding — 30 bytes of fields rounded up to
/// 32 by the outer u32's alignment — and padding is uninitialised memory that
/// Rust makes no promise about. Copying it into a blob hands the kernel two
/// bytes of whatever was on the stack, and reading it back is undefined
/// behaviour proper: the layout test did exactly that, and once the optimiser
/// could see across it the test process died on an illegal instruction rather
/// than failing.
///
/// Little-endian throughout, which is what `drm_mode.h` means by these types:
/// the kernel copies the bytes into the infoframe unchanged.
fn hdr_metadata_bytes() -> [u8; std::mem::size_of::<HdrOutputMetadata>()] {
    let mut bytes = [0u8; std::mem::size_of::<HdrOutputMetadata>()];
    // metadata_type is the leading u32 and stays zero: the only type the
    // kernel defines. Then the infoframe, at the offset that u32 leaves it.
    let infoframe = std::mem::offset_of!(HdrOutputMetadata, hdmi_metadata_type1);
    bytes[infoframe] = EOTF_ST2084;
    // The byte after it is the static metadata descriptor, type 1, which is
    // zero. Everything past that is the mastering display's primaries, white
    // point and luminances — left zero, meaning unset, so the display's own
    // capabilities are used rather than numbers this compositor would be
    // inventing.
    bytes
}

/// A connector property by name, with its current value.
fn find_property(
    device: &impl ControlDevice,
    connector: connector::Handle,
    name: &str,
) -> Option<(property::Handle, property::ValueType, property::RawValue)> {
    let props = device.get_properties(connector).ok()?;
    for (handle, value) in props.iter() {
        let Ok(info) = device.get_property(*handle) else {
            continue;
        };
        if info.name().to_str().ok()? == name {
            return Some((*handle, info.value_type(), *value));
        }
    }
    None
}

/// The value of an enum property by the name of its variant.
///
/// Enum values are not fixed across drivers, so the name is what has to be
/// matched — hardcoding a number works on one kernel and picks something else
/// on the next.
fn enum_value(
    device: &impl ControlDevice,
    handle: property::Handle,
    want: &str,
) -> Option<property::RawValue> {
    let info = device.get_property(handle).ok()?;
    let property::ValueType::Enum(values) = info.value_type() else {
        return None;
    };
    let (raw, named) = values.values();
    raw.iter()
        .zip(named.iter())
        .find(|(_, entry)| entry.name().to_str().map(|n| n == want).unwrap_or(false))
        .map(|(value, _)| *value)
}

/// Whether this connector can be driven in HDR at all.
///
/// Both properties have to exist. A display with `Colorspace` and no
/// `HDR_OUTPUT_METADATA` would take the primaries and none of the curve, which
/// is worse than staying in SDR.
pub fn capable(device: &impl ControlDevice, connector: connector::Handle) -> bool {
    let colorspace = find_property(device, connector, "Colorspace");
    let metadata = find_property(device, connector, "HDR_OUTPUT_METADATA");
    let Some((handle, _, _)) = colorspace else {
        return false;
    };
    if metadata.is_none() {
        return false;
    }
    // And it has to offer the BT.2020 variant, not merely have the property.
    enum_value(device, handle, "BT2020_RGB").is_some()
}

/// What to commit for an HDR change, or `None` if this connector cannot.
///
/// Returns the property handles and values rather than committing, so the
/// caller can put them in the same atomic commit as everything else — and so
/// this is testable without a display.
pub fn properties(
    device: &impl ControlDevice,
    connector: connector::Handle,
    enabled: bool,
) -> Option<(Vec<property::Handle>, Vec<property::RawValue>, bool)> {
    let (colorspace, _, _) = find_property(device, connector, "Colorspace")?;
    let (metadata, _, _) = find_property(device, connector, "HDR_OUTPUT_METADATA")?;

    // Leaving HDR names the default rather than sRGB: that hands the output
    // back to whatever its default is, which is not necessarily sRGB and is
    // not this compositor's to decide (`src/hdr.c:124`).
    let want = if enabled { "BT2020_RGB" } else { "Default" };
    let colorspace_value = enum_value(device, colorspace, want)?;

    Some((vec![colorspace, metadata], vec![colorspace_value, 0], enabled))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_metadata_blob_is_the_size_the_kernel_expects() {
        // The kernel reads these bytes directly. A struct that is the wrong
        // size is refused; one that is the right size with the fields in the
        // wrong order is not — it is simply believed.
        //
        // 4 for the outer type, then the infoframe: two bytes of eotf and
        // descriptor, six coordinates of four bytes, and four luminance
        // fields — 30, rounded to 32 by the u32's alignment.
        assert_eq!(std::mem::size_of::<HdrMetadataInfoframe>(), 26);
        assert_eq!(std::mem::size_of::<HdrOutputMetadata>(), 32);
    }

    #[test]
    fn the_blob_asks_for_pq_and_leaves_the_rest_unset() {
        let bytes = hdr_metadata_bytes();
        assert_eq!(bytes.len(), 32);
        // metadata_type is the first u32 and must be zero: it is the only type
        // the kernel defines.
        assert_eq!(&bytes[0..4], &[0, 0, 0, 0]);
        // Then eotf, which is 2 for ST 2084.
        assert_eq!(bytes[4], EOTF_ST2084);
        // Static metadata type 1.
        assert_eq!(bytes[5], 0);
        // Everything after is the mastering display's own numbers, left unset
        // so the display's capabilities are used rather than invented ones.
        assert!(bytes[6..].iter().all(|b| *b == 0), "luminance was invented");
    }
}

/// Switch a connector into or out of HDR.
///
/// One atomic commit carrying both properties, tested before it is kept: a
/// display that refuses must be left showing what it was showing rather than
/// going dark while someone works out why (`src/hdr.c:145`).
pub fn set(
    device: &impl ControlDevice,
    connector: connector::Handle,
    enabled: bool,
) -> anyhow::Result<()> {
    use smithay::reexports::drm::control::atomic::AtomicModeReq;
    use smithay::reexports::drm::control::AtomicCommitFlags;

    let Some((handles, values, wants_blob)) = properties(device, connector, enabled) else {
        anyhow::bail!("this display has no Colorspace or HDR_OUTPUT_METADATA property");
    };

    // Zero clears the metadata, which is what leaving HDR wants.
    let blob = if wants_blob {
        let metadata = hdr_metadata_bytes();
        match device.create_property_blob(&metadata) {
            Ok(property::Value::Blob(id)) => Some(id),
            Ok(_) => anyhow::bail!("the kernel returned something that is not a blob"),
            Err(e) => anyhow::bail!("creating the HDR metadata blob: {e}"),
        }
    } else {
        None
    };

    let mut request = AtomicModeReq::new();
    request.add_raw_property(connector.into(), handles[0], values[0]);
    request.add_raw_property(connector.into(), handles[1], blob.unwrap_or(0));

    // Tested first, and only committed if the test passed. A change the
    // hardware will not take must leave the monitor alone.
    let result = device
        .atomic_commit(
            AtomicCommitFlags::TEST_ONLY | AtomicCommitFlags::ALLOW_MODESET,
            request.clone(),
        )
        .and_then(|()| device.atomic_commit(AtomicCommitFlags::ALLOW_MODESET, request));

    // The blob is the kernel's now if the commit landed, and ours to drop if
    // it did not; destroying it after a successful commit is correct either
    // way, because the kernel holds its own reference.
    if let Some(id) = blob {
        let _ = device.destroy_property_blob(id);
    }

    result.map_err(|e| anyhow::anyhow!("the display refused HDR: {e}"))
}
