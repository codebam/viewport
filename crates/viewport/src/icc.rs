// SPDX-License-Identifier: GPL-3.0-or-later
//
// The `vcgt` tag of an ICC profile: the calibration ramp a profiler stored
// for the video card to load.
//
// Hyprland's `render:icc_vcgt_enabled`, and the one part of ICC support that
// is a table rather than a colour transform. A monitor profile made by a
// colorimeter carries both — a description of how the display responds, and
// the ramp that linearises it — and the ramp is what a compositor can apply
// without a colour-management engine.
//
// The tag is private to Apple (ColorSync 2.5) and every implementation reads
// it slightly differently, which is why the table offset below is tried both
// ways. `xcalib` is the reference that settled it.

use crate::gamma::Ramp;

/// The `vcgt` tag signature, big-endian.
const VCGT: u32 = u32::from_be_bytes(*b"vcgt");

/// Read the calibration ramp out of an ICC profile, if it has one.
pub fn vcgt(profile: &[u8]) -> Option<Ramp> {
    // 128-byte header, then a u32 tag count, then 12 bytes per tag.
    if profile.len() < 132 {
        return None;
    }
    let count = be_u32(&profile[128..132]) as usize;
    // A count that does not fit the file is a corrupt profile, not a large
    // one: every entry is twelve bytes and they all live after the count.
    if count > (profile.len() - 132) / 12 {
        return None;
    }
    for at in (132..132 + count * 12).step_by(12) {
        if be_u32(&profile[at..at + 4]) != VCGT {
            continue;
        }
        let offset = be_u32(&profile[at + 4..at + 8]) as usize;
        let size = be_u32(&profile[at + 8..at + 12]) as usize;
        let end = offset.checked_add(size)?;
        if end > profile.len() {
            return None;
        }
        return parse_tag(&profile[offset..end]);
    }
    None
}

fn parse_tag(tag: &[u8]) -> Option<Ramp> {
    if tag.len() < 12 || be_u32(&tag[0..4]) != VCGT {
        return None;
    }
    match be_u32(&tag[8..12]) {
        // VideoCardGammaTable
        0 => parse_table(tag),
        // VideoCardGammaFormula
        1 => parse_formula(tag),
        _ => None,
    }
}

fn parse_table(tag: &[u8]) -> Option<Ramp> {
    if tag.len() < 18 {
        return None;
    }
    let channels = be_u16(&tag[12..14]) as usize;
    let entries = be_u16(&tag[14..16]) as usize;
    let entry_size = be_u16(&tag[16..18]) as usize;
    if channels != 3 || entries == 0 || !matches!(entry_size, 1 | 2) {
        return None;
    }
    let table_bytes = channels.checked_mul(entries)?.checked_mul(entry_size)?;

    // Two writers disagree about the two reserved bytes after the header: one
    // starts the table at 20, the other at 18. A ramp read at the wrong offset
    // is still shaped like a ramp, so the check is that its top entry reaches
    // near full scale — which every real calibration does and the misread
    // does not.
    let mut fallback = None;
    for start in [20usize, 18] {
        if tag.len() < start + table_bytes {
            continue;
        }
        let read = |channel: usize| -> Vec<u16> {
            (0..entries)
                .map(|i| {
                    let at = start + (channel * entries + i) * entry_size;
                    match entry_size {
                        1 => u16::from(tag[at]) << 8,
                        _ => be_u16(&tag[at..at + 2]),
                    }
                })
                .collect()
        };
        let ramp = Ramp {
            red: read(0),
            green: read(1),
            blue: read(2),
        };
        if ramp.red.last().copied().unwrap_or(0) >= 30_000 {
            return Some(ramp);
        }
        fallback.get_or_insert(ramp);
    }
    fallback
}

fn parse_formula(tag: &[u8]) -> Option<Ramp> {
    // Three channels of (gamma, min, max), each S15Fixed16.
    if tag.len() < 12 + 36 {
        return None;
    }
    let fixed = |at: usize| -> f64 { f64::from(be_i32(&tag[at..at + 4])) / 65536.0 };
    let channel = |c: usize| -> (f64, f64, f64) {
        let at = 12 + c * 12;
        (fixed(at), fixed(at + 4), fixed(at + 8))
    };
    let (rg, rmin, rmax) = channel(0);
    let (gg, gmin, gmax) = channel(1);
    let (bg, bmin, bmax) = channel(2);
    for (gamma, min, max) in [(rg, rmin, rmax), (gg, gmin, gmax), (bg, bmin, bmax)] {
        // A formula that is not a gamma curve is a profile this cannot use;
        // refusing is better than loading a ramp that makes the screen worse.
        if !(0.0 < gamma && gamma <= 5.0)
            || !(0.0..1.0).contains(&min)
            || !(0.0..=1.0).contains(&max)
        {
            return None;
        }
    }

    let size = 256usize;
    let sample = |gamma: f64, min: f64, max: f64, i: usize| -> u16 {
        let x = i as f64 / (size as f64 - 1.0);
        let y = min + (max - min) * x.powf(gamma);
        (y.clamp(0.0, 1.0) * f64::from(u16::MAX)).round() as u16
    };
    Some(Ramp {
        red: (0..size).map(|i| sample(rg, rmin, rmax, i)).collect(),
        green: (0..size).map(|i| sample(gg, gmin, gmax, i)).collect(),
        blue: (0..size).map(|i| sample(bg, bmin, bmax, i)).collect(),
    })
}

fn be_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn be_i32(bytes: &[u8]) -> i32 {
    i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn be_u16(bytes: &[u8]) -> u16 {
    u16::from_be_bytes([bytes[0], bytes[1]])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A profile with one tag, laid out the way a real one is.
    fn profile(signature: &[u8; 4], tag: &[u8]) -> Vec<u8> {
        let offset = 132 + 12;
        let mut bytes = vec![0u8; offset];
        bytes[128..132].copy_from_slice(&1u32.to_be_bytes());
        bytes[132..136].copy_from_slice(signature);
        bytes[136..140].copy_from_slice(&(offset as u32).to_be_bytes());
        bytes[140..144].copy_from_slice(&(tag.len() as u32).to_be_bytes());
        bytes.extend_from_slice(tag);
        bytes
    }

    fn table_tag(start_padding: bool, entry_size: u16) -> Vec<u8> {
        let entries = 4u16;
        let mut tag = Vec::new();
        tag.extend_from_slice(b"vcgt");
        tag.extend_from_slice(&0u32.to_be_bytes());
        tag.extend_from_slice(&0u32.to_be_bytes());
        tag.extend_from_slice(&3u16.to_be_bytes());
        tag.extend_from_slice(&entries.to_be_bytes());
        tag.extend_from_slice(&entry_size.to_be_bytes());
        if start_padding {
            tag.extend_from_slice(&0u16.to_be_bytes());
        }
        for channel in 0..3u16 {
            for i in 0..entries {
                let value = (channel * entries + i) * 1000;
                if entry_size == 1 {
                    tag.push((value >> 8) as u8);
                } else {
                    tag.extend_from_slice(&value.to_be_bytes());
                }
            }
        }
        tag
    }

    #[test]
    fn a_vcgt_table_reads_back_channel_by_channel() {
        let bytes = profile(b"vcgt", &table_tag(true, 2));
        let ramp = vcgt(&bytes).expect("a table");
        assert_eq!(ramp.red, vec![0, 1000, 2000, 3000]);
        assert_eq!(ramp.green, vec![4000, 5000, 6000, 7000]);
        assert_eq!(ramp.blue, vec![8000, 9000, 10000, 11000]);
    }

    #[test]
    fn the_eight_bit_entry_size_is_scaled_to_sixteen() {
        // A Mac-created profile stores one byte per entry; the ramp is
        // 16-bit, so the value is the byte widened, not divided.
        let bytes = profile(b"vcgt", &table_tag(true, 1));
        let ramp = vcgt(&bytes).expect("a table");
        assert_eq!(ramp.red, vec![0, 0x0300, 0x0700, 0x0b00]);
    }

    #[test]
    fn a_table_without_the_reserved_word_is_read_too() {
        // The writers that start the table at 18 rather than 20. The top entry
        // has to reach full scale or the reader tries the other offset.
        let mut tag = table_tag(false, 2);
        // Overwrite the last entry of the blue channel with a full-scale value
        // so this reads as a real calibration at offset 18.
        let at = tag.len() - 2;
        tag[at..].copy_from_slice(&65535u16.to_be_bytes());
        let bytes = profile(b"vcgt", &tag);
        let ramp = vcgt(&bytes).expect("a table");
        assert_eq!(ramp.blue.last().copied(), Some(65535));
    }

    #[test]
    fn a_vcgt_formula_reads_back_a_curve() {
        let mut tag = Vec::new();
        tag.extend_from_slice(b"vcgt");
        tag.extend_from_slice(&0u32.to_be_bytes());
        tag.extend_from_slice(&1u32.to_be_bytes());
        // gamma 2.2, min 0, max 1 for every channel.
        for _ in 0..3 {
            tag.extend_from_slice(&((2.2 * 65536.0) as i32).to_be_bytes());
            tag.extend_from_slice(&0i32.to_be_bytes());
            tag.extend_from_slice(&65536i32.to_be_bytes());
        }
        let bytes = profile(b"vcgt", &tag);
        let ramp = vcgt(&bytes).expect("a formula");
        assert_eq!(ramp.red.len(), 256);
        assert_eq!(ramp.red[0], 0);
        assert_eq!(ramp.red[255], 65535);
        // Mid-scale of a 2.2 curve is about 0.22 of full scale.
        let middle = f64::from(ramp.red[128]) / 65535.0;
        assert!((0.20..0.25).contains(&middle), "{middle}");
    }

    #[test]
    fn a_profile_with_no_vcgt_tag_has_no_ramp() {
        let bytes = profile(b"wtpt", &[0u8; 16]);
        assert!(vcgt(&bytes).is_none());
    }

    #[test]
    fn a_truncated_profile_is_refused_rather_than_read_past() {
        assert!(vcgt(&[]).is_none());
        assert!(vcgt(&[0u8; 100]).is_none());
        let mut bytes = profile(b"vcgt", &table_tag(true, 2));
        bytes.truncate(bytes.len() - 3);
        assert!(vcgt(&bytes).is_none());
    }

    #[test]
    fn a_formula_out_of_range_is_refused() {
        let mut tag = Vec::new();
        tag.extend_from_slice(b"vcgt");
        tag.extend_from_slice(&0u32.to_be_bytes());
        tag.extend_from_slice(&1u32.to_be_bytes());
        for _ in 0..3 {
            tag.extend_from_slice(&((40.0 * 65536.0) as i32).to_be_bytes());
            tag.extend_from_slice(&0i32.to_be_bytes());
            tag.extend_from_slice(&65536i32.to_be_bytes());
        }
        let bytes = profile(b"vcgt", &tag);
        assert!(vcgt(&bytes).is_none());
    }
}
