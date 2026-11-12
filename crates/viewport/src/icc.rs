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

// ---------------------------------------------------------------------------
// The display transform
// ---------------------------------------------------------------------------

/// Samples per transfer curve. The curve tags are either a table or a formula
/// and both end up here, so one interpolation serves every profile.
const CURVE_SAMPLES: usize = 1024;

/// The side of the cube the renderer samples. Three points over the 8-bit
/// value is about the point where a straight line between two neighbours stops
/// being visible, and 33³ floats is 143 KiB — small enough to upload once per
/// profile and keep.
pub const LUT_SIZE: usize = 33;

/// A 3×3 matrix, row-major.
type Mat3 = [[f64; 3]; 3];

/// sRGB's primaries against D65, the space the shell and most clients draw in.
const SRGB_TO_XYZ_D65: Mat3 = [
    [0.412_456_4, 0.357_576_1, 0.180_437_5],
    [0.212_672_9, 0.715_152_2, 0.072_175_0],
    [0.019_333_9, 0.119_192_0, 0.950_304_1],
];

/// The ICC's PCS is D50, so sRGB has to be adapted before it meets a profile's
/// matrix. Bradford is the adaptation the specification names.
const BRADFORD_D65_TO_D50: Mat3 = [
    [1.047_882, 0.022_919, -0.050_201],
    [0.029_587, 0.990_479, -0.017_059],
    [-0.009_232, 0.015_076, 0.751_930],
];

/// What to send a monitor so that an sRGB picture looks like sRGB.
///
/// A matrix-shaper display profile is three primaries (rXYZ, gXYZ, bXYZ, as PCS
/// XYZ relative to D50) and three transfer curves (rTRC, gTRC, bTRC, device
/// value to linear light). Reading it backwards gives the transform the screen
/// needs: sRGB encoded → linear sRGB → PCS D50 → linear display → display
/// encoded. `lut` is that function sampled on a cube, which is the form the
/// renderer can upload as a 3D texture.
///
/// Profiles whose description is a LUT (`A2B0`/`B2A0`) are refused rather than
/// half-read: their forward direction is a table, and inverting that is a
/// different piece of work.
pub struct DisplayTransform {
    /// display encoded → linear light, per channel.
    decode: [Curve; 3],
    /// PCS XYZ D50 → linear display RGB.
    from_pcs: Mat3,
    /// sRGB encoded → PCS XYZ D50.
    srgb_to_pcs: Mat3,
    /// `r + g*N + b*N²` entries, each `[r, g, b, 1.0]`, display encoded.
    lut: Vec<[f32; 4]>,
}

impl DisplayTransform {
    /// Read a matrix-shaper display profile. Anything else is `None`: a
    /// profile that cannot be understood is not a reason to guess a transform.
    pub fn for_profile(profile: &[u8]) -> Option<Self> {
        if profile.len() < 132 || &profile[36..40] != b"acsp" {
            return None;
        }
        // The transform is RGB in, RGB out; an XYZ or Lab device space is a
        // different question with a different answer.
        if &profile[16..20] != b"RGB " || &profile[20..24] != b"XYZ " {
            return None;
        }
        let r = xyz_tag(tag_data(profile, b"rXYZ")?)?;
        let g = xyz_tag(tag_data(profile, b"gXYZ")?)?;
        let b = xyz_tag(tag_data(profile, b"bXYZ")?)?;
        let to_pcs = [[r[0], g[0], b[0]], [r[1], g[1], b[1]], [r[2], g[2], b[2]]];
        let from_pcs = invert(&to_pcs)?;
        let decode = [
            Curve::from_tag(tag_data(profile, b"rTRC")?)?,
            Curve::from_tag(tag_data(profile, b"gTRC")?)?,
            Curve::from_tag(tag_data(profile, b"bTRC")?)?,
        ];
        let mut transform = Self {
            decode,
            from_pcs,
            srgb_to_pcs: multiply(&BRADFORD_D65_TO_D50, &SRGB_TO_XYZ_D65),
            lut: Vec::new(),
        };
        transform.lut = transform.bake();
        Some(transform)
    }

    /// The transform itself: an sRGB encoded colour in, the display's encoded
    /// colour out. Out-of-gamut colours are clipped, which is what a screen
    /// with no room for them can do.
    pub fn map(&self, srgb: [f64; 3]) -> [f64; 3] {
        let linear = [
            srgb_decode(srgb[0]),
            srgb_decode(srgb[1]),
            srgb_decode(srgb[2]),
        ];
        let pcs = mul_vec(&self.srgb_to_pcs, &linear);
        let device = mul_vec(&self.from_pcs, &pcs);
        let mut out = [0.0; 3];
        for (channel, value) in out.iter_mut().enumerate() {
            *value = self.decode[channel]
                .invert(device[channel].clamp(0.0, 1.0))
                .clamp(0.0, 1.0);
        }
        out
    }

    /// The transform on a cube, in the form a 3D texture wants: r varies
    /// fastest, then g, then b, and alpha is full because the cube is colour.
    pub fn lut(&self) -> &[[f32; 4]] {
        &self.lut
    }

    fn bake(&self) -> Vec<[f32; 4]> {
        let n = LUT_SIZE;
        let step = 1.0 / (n - 1) as f64;
        let mut lut = Vec::with_capacity(n * n * n);
        for blue in 0..n {
            for green in 0..n {
                for red in 0..n {
                    let mapped =
                        self.map([red as f64 * step, green as f64 * step, blue as f64 * step]);
                    lut.push([mapped[0] as f32, mapped[1] as f32, mapped[2] as f32, 1.0]);
                }
            }
        }
        lut
    }
}

/// A transfer curve, sampled: encoded value to linear light. The inverse is a
/// binary search over the same samples, so there is one table to keep right.
struct Curve {
    samples: Vec<f64>,
}

impl Curve {
    fn identity() -> Option<Self> {
        let samples: Vec<f64> = (0..CURVE_SAMPLES)
            .map(|i| i as f64 / (CURVE_SAMPLES - 1) as f64)
            .collect();
        Self::from_samples(samples)
    }

    fn gamma(gamma: f64) -> Option<Self> {
        if !(gamma.is_finite() && gamma > 0.0) {
            return None;
        }
        let samples: Vec<f64> = (0..CURVE_SAMPLES)
            .map(|i| (i as f64 / (CURVE_SAMPLES - 1) as f64).powf(gamma))
            .collect();
        Self::from_samples(samples)
    }

    fn from_samples(samples: Vec<f64>) -> Option<Self> {
        if samples.len() < 2 || samples.iter().any(|value| !value.is_finite()) {
            return None;
        }
        // A curve that goes backwards cannot be inverted, and one that barely
        // moves has no usable inverse either — both are profiles this refuses
        // rather than a screen made worse.
        for pair in samples.windows(2) {
            if pair[1] + 1e-6 < pair[0] {
                return None;
            }
        }
        let last = *samples.last()?;
        let first = samples[0];
        if last - first < 0.05 {
            return None;
        }
        Some(Self {
            samples: resample(&samples),
        })
    }

    fn from_tag(data: &[u8]) -> Option<Self> {
        match &data[0..4] {
            b"curv" => Self::from_curv(data),
            b"para" => Self::from_para(data),
            _ => None,
        }
    }

    fn from_curv(data: &[u8]) -> Option<Self> {
        if data.len() < 12 {
            return None;
        }
        let count = be_u32(&data[8..12]) as usize;
        match count {
            // An empty table is the identity, by the specification.
            0 => Self::identity(),
            // A single entry is a gamma in 8.8 fixed point.
            1 => {
                if data.len() < 14 {
                    return None;
                }
                let gamma = f64::from(be_u16(&data[12..14])) / 256.0;
                Self::gamma(gamma)
            }
            count => {
                let bytes = count.checked_mul(2)?;
                if data.len() < 12 + bytes {
                    return None;
                }
                let samples = (0..count)
                    .map(|i| f64::from(be_u16(&data[12 + 2 * i..14 + 2 * i])) / 65535.0)
                    .collect();
                Self::from_samples(samples)
            }
        }
    }

    fn from_para(data: &[u8]) -> Option<Self> {
        // type (u16), reserved (u16), then up to seven S15Fixed16 parameters.
        if data.len() < 12 {
            return None;
        }
        let kind = be_u16(&data[8..10]);
        let count = match kind {
            0 => 1,
            1 => 3,
            2 => 4,
            3 => 5,
            4 => 7,
            _ => return None,
        };
        if data.len() < 12 + count * 4 {
            return None;
        }
        let parameter = |index: usize| -> f64 {
            f64::from(be_i32(&data[12 + index * 4..16 + index * 4])) / 65536.0
        };
        let p: Vec<f64> = (0..count).map(parameter).collect();
        if p.iter().any(|value| !value.is_finite()) {
            return None;
        }
        let samples: Vec<f64> = (0..CURVE_SAMPLES)
            .map(|i| {
                let x = i as f64 / (CURVE_SAMPLES - 1) as f64;
                para_value(kind, &p, x)
            })
            .collect();
        Self::from_samples(samples)
    }

    /// The encoded value that produces `linear`.
    fn invert(&self, linear: f64) -> f64 {
        let linear = linear.clamp(0.0, 1.0);
        // Both tables are monotone, so a binary search is the inverse; the
        // interpolation keeps it smooth between samples.
        let mut low = 0usize;
        let mut high = CURVE_SAMPLES - 1;
        while high - low > 1 {
            let middle = (low + high) / 2;
            if self.samples[middle] <= linear {
                low = middle;
            } else {
                high = middle;
            }
        }
        let low_value = self.samples[low];
        let high_value = self.samples[high];
        if high_value <= low_value {
            return low as f64 / (CURVE_SAMPLES - 1) as f64;
        }
        let fraction = ((linear - low_value) / (high_value - low_value)).clamp(0.0, 1.0);
        (low as f64 + fraction) / (CURVE_SAMPLES - 1) as f64
    }
}

/// One of the ICC's parametric curve functions, evaluated at `x`.
fn para_value(kind: u16, p: &[f64], x: f64) -> f64 {
    let (g, a, b) = (p[0], *p.get(1).unwrap_or(&1.0), *p.get(2).unwrap_or(&0.0));
    match kind {
        1 => {
            let knee = if a != 0.0 { -b / a } else { 0.0 };
            if x >= knee {
                (a * x + b).max(0.0).powf(g)
            } else {
                0.0
            }
        }
        2 => {
            let knee = if a != 0.0 { -b / a } else { 0.0 };
            let offset = p.get(3).copied().unwrap_or(0.0);
            if x >= knee {
                (a * x + b).max(0.0).powf(g) + offset
            } else {
                offset
            }
        }
        3 => {
            let c = p[3];
            let d = p[4];
            if x >= d {
                (a * x + b).max(0.0).powf(g)
            } else {
                c * x
            }
        }
        4 => {
            let c = p[3];
            let d = p[4];
            let e = p[5];
            let f = p[6];
            if x >= d {
                (a * x + b).max(0.0).powf(g) + e
            } else {
                c * x + f
            }
        }
        _ => x.max(0.0).powf(g),
    }
}

/// Spread an arbitrary sample count over everything [`Curve`] works in.
fn resample(samples: &[f64]) -> Vec<f64> {
    if samples.len() == CURVE_SAMPLES {
        return samples.to_vec();
    }
    (0..CURVE_SAMPLES)
        .map(|i| {
            let at = i as f64 / (CURVE_SAMPLES - 1) as f64 * (samples.len() - 1) as f64;
            let index = (at.floor() as usize).min(samples.len() - 2);
            let fraction = at - index as f64;
            samples[index] * (1.0 - fraction) + samples[index + 1] * fraction
        })
        .collect()
}

/// sRGB's transfer function: encoded → linear light.
fn srgb_decode(value: f64) -> f64 {
    let value = value.clamp(0.0, 1.0);
    if value <= 0.040_45 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

/// Find one tag's data in a profile.
fn tag_data<'a>(profile: &'a [u8], signature: &[u8; 4]) -> Option<&'a [u8]> {
    let count = be_u32(&profile[128..132]) as usize;
    if count > (profile.len() - 132) / 12 {
        return None;
    }
    let wanted = u32::from_be_bytes(*signature);
    for at in (132..132 + count * 12).step_by(12) {
        if be_u32(&profile[at..at + 4]) != wanted {
            continue;
        }
        let offset = be_u32(&profile[at + 4..at + 8]) as usize;
        let size = be_u32(&profile[at + 8..at + 12]) as usize;
        let end = offset.checked_add(size)?;
        if size == 0 || end > profile.len() {
            return None;
        }
        return Some(&profile[offset..end]);
    }
    None
}

/// An `XYZ ` tag: three S15Fixed16 values, relative to the D50 PCS.
fn xyz_tag(data: &[u8]) -> Option<[f64; 3]> {
    if data.len() < 20 || &data[0..4] != b"XYZ " {
        return None;
    }
    let value = |at: usize| f64::from(be_i32(&data[at..at + 4])) / 65536.0;
    Some([value(8), value(12), value(16)])
}

fn mul_vec(matrix: &Mat3, vector: &[f64; 3]) -> [f64; 3] {
    [
        matrix[0][0] * vector[0] + matrix[0][1] * vector[1] + matrix[0][2] * vector[2],
        matrix[1][0] * vector[0] + matrix[1][1] * vector[1] + matrix[1][2] * vector[2],
        matrix[2][0] * vector[0] + matrix[2][1] * vector[1] + matrix[2][2] * vector[2],
    ]
}

fn multiply(a: &Mat3, b: &Mat3) -> Mat3 {
    let mut out = [[0.0; 3]; 3];
    for (row, out_row) in out.iter_mut().enumerate() {
        for (column, out_value) in out_row.iter_mut().enumerate() {
            *out_value = (0..3).map(|k| a[row][k] * b[k][column]).sum();
        }
    }
    out
}

fn invert(matrix: &Mat3) -> Option<Mat3> {
    let m = matrix;
    let determinant = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
        - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
    // A singular matrix is a profile with no answer, not one with a zero
    // transform: refusing keeps the screen as it was instead of blacking it.
    if !determinant.is_finite() || determinant.abs() < 1e-9 {
        return None;
    }
    let scale = determinant.recip();
    Some([
        [
            (m[1][1] * m[2][2] - m[1][2] * m[2][1]) * scale,
            (m[0][2] * m[2][1] - m[0][1] * m[2][2]) * scale,
            (m[0][1] * m[1][2] - m[0][2] * m[1][1]) * scale,
        ],
        [
            (m[1][2] * m[2][0] - m[1][0] * m[2][2]) * scale,
            (m[0][0] * m[2][2] - m[0][2] * m[2][0]) * scale,
            (m[0][2] * m[1][0] - m[0][0] * m[1][2]) * scale,
        ],
        [
            (m[1][0] * m[2][1] - m[1][1] * m[2][0]) * scale,
            (m[0][1] * m[2][0] - m[0][0] * m[2][1]) * scale,
            (m[0][0] * m[1][1] - m[0][1] * m[1][0]) * scale,
        ],
    ])
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

#[cfg(test)]
mod transform_tests {
    use super::*;

    /// The sRGB primaries adapted to D50, the matrix a profile for an sRGB
    /// screen carries.
    const SRGB_D50: Mat3 = [
        [0.436_074_7, 0.385_064_9, 0.143_080_4],
        [0.222_504_5, 0.716_878_6, 0.060_616_9],
        [0.013_932_2, 0.097_104_5, 0.714_173_3],
    ];

    /// Rec. 2020's primaries adapted to D50.
    const REC2020_D50: Mat3 = [
        [0.673_478, 0.165_671, 0.125_064],
        [0.279_045, 0.675_343, 0.045_634],
        [-0.001_920, 0.029_995, 0.797_121],
    ];

    fn fixed(value: f64) -> [u8; 4] {
        ((value * 65536.0).round() as i32).to_be_bytes()
    }

    fn xyz_tag(value: [f64; 3]) -> Vec<u8> {
        let mut data = b"XYZ \0\0\0\0".to_vec();
        for component in value {
            data.extend_from_slice(&fixed(component));
        }
        data
    }

    fn curv_gamma(gamma: f64) -> Vec<u8> {
        let mut data = b"curv\0\0\0\0".to_vec();
        data.extend_from_slice(&1u32.to_be_bytes());
        data.extend_from_slice(&((gamma * 256.0).round() as u16).to_be_bytes());
        data
    }

    fn para_type3(g: f64, a: f64, b: f64, c: f64, d: f64) -> Vec<u8> {
        let mut data = b"para\0\0\0\0".to_vec();
        data.extend_from_slice(&3u16.to_be_bytes());
        data.extend_from_slice(&0u16.to_be_bytes());
        for parameter in [g, a, b, c, d] {
            data.extend_from_slice(&fixed(parameter));
        }
        data
    }

    /// The sRGB transfer function as an ICC parametric curve, type 3.
    fn srgb_curve() -> Vec<u8> {
        para_type3(2.4, 1.0 / 1.055, 0.055 / 1.055, 1.0 / 12.92, 0.040_45)
    }

    /// A matrix-and-curve profile, laid out the way a real one is.
    fn profile(matrix: &Mat3, curve: &[u8]) -> Vec<u8> {
        let column = |index: usize| [matrix[0][index], matrix[1][index], matrix[2][index]];
        let mut tags: Vec<(&[u8; 4], Vec<u8>)> = vec![
            (b"wtpt", xyz_tag([0.9642, 1.0, 0.8249])),
            (b"rXYZ", xyz_tag(column(0))),
            (b"gXYZ", xyz_tag(column(1))),
            (b"bXYZ", xyz_tag(column(2))),
            (b"rTRC", curve.to_vec()),
            (b"gTRC", curve.to_vec()),
            (b"bTRC", curve.to_vec()),
        ];
        tags.sort_by(|a, b| a.0.cmp(b.0));

        let mut header = vec![0u8; 128];
        header[4..8].copy_from_slice(b"test");
        header[8..12].copy_from_slice(&0x0430_0000u32.to_be_bytes());
        header[12..16].copy_from_slice(b"mntr");
        header[16..20].copy_from_slice(b"RGB ");
        header[20..24].copy_from_slice(b"XYZ ");
        header[36..40].copy_from_slice(b"acsp");
        header[68..72].copy_from_slice(&fixed(0.9642));
        header[72..76].copy_from_slice(&fixed(1.0));
        header[76..80].copy_from_slice(&fixed(0.8249));

        let mut body = Vec::new();
        let mut table = Vec::new();
        let mut offset = 132 + 12 * tags.len();
        for (signature, data) in &tags {
            while !offset.is_multiple_of(4) {
                body.push(0);
                offset += 1;
            }
            table.extend_from_slice(*signature);
            table.extend_from_slice(&(offset as u32).to_be_bytes());
            table.extend_from_slice(&(data.len() as u32).to_be_bytes());
            body.extend_from_slice(data);
            offset += data.len();
        }
        let total = (128 + 4 + table.len() + body.len()) as u32;
        header[0..4].copy_from_slice(&total.to_be_bytes());

        let mut out = header;
        out.extend_from_slice(&(tags.len() as u32).to_be_bytes());
        out.extend_from_slice(&table);
        out.extend_from_slice(&body);
        out
    }

    /// Every colour is left alone by a profile that describes sRGB itself.
    #[test]
    fn an_srgb_profile_is_the_identity() {
        let transform =
            DisplayTransform::for_profile(&profile(&SRGB_D50, &srgb_curve())).expect("a transform");
        for colour in [
            [0.0, 0.0, 0.0],
            [0.05, 0.5, 0.95],
            [0.501_960_8, 0.501_960_8, 0.501_960_8],
            [1.0, 1.0, 1.0],
        ] {
            let mapped = transform.map(colour);
            for channel in 0..3 {
                assert!(
                    (mapped[channel] - colour[channel]).abs() < 3e-3,
                    "{colour:?} became {mapped:?}"
                );
            }
        }
    }

    /// A gamma-2.2 display needs the encoding re-done, and the answer is the
    /// formula: decode sRGB, encode gamma 2.2.
    #[test]
    fn a_gamma_display_changes_the_encoding_curve() {
        let curve = curv_gamma(2.2);
        let transform =
            DisplayTransform::for_profile(&profile(&SRGB_D50, &curve)).expect("a transform");
        let mapped = transform.map([0.5, 0.5, 0.5]);
        let expected = srgb_decode(0.5).powf(1.0 / 2.2);
        for channel in 0..3 {
            assert!(
                (mapped[channel] - expected).abs() < 3e-3,
                "grey became {mapped:?}, expected {expected}"
            );
        }
    }

    /// A wide-gamut display: these numbers are LittleCMS 2.19's own answer for
    /// the same profile (sRGB in, relative colorimetric), converted from its
    /// 8-bit output. They are the check that this is a colour transform and
    /// not a plausible-looking one.
    #[test]
    fn a_wide_gamut_display_matches_lcms2() {
        let curve = curv_gamma(2.2);
        let transform =
            DisplayTransform::for_profile(&profile(&REC2020_D50, &curve)).expect("a transform");
        let expected = [
            // sRGB red, green, blue
            ([1.0, 0.0, 0.0], [0.809, 0.296_7, 0.154_2]),
            ([0.0, 1.0, 0.0], [0.603_5, 0.962_6, 0.331_1]),
            ([0.0, 0.0, 1.0], [0.240, 0.130_4, 0.951]),
            // a neutral and a mixed colour
            (
                [0.501_960_8, 0.501_960_8, 0.501_960_8],
                [0.498, 0.498, 0.498],
            ),
            ([0.2, 0.4, 0.6], [0.314_1, 0.392_8, 0.576_1]),
        ];
        for (input, wanted) in expected {
            let mapped = transform.map(input);
            for channel in 0..3 {
                assert!(
                    (mapped[channel] - wanted[channel]).abs() < 3e-3,
                    "{input:?} became {mapped:?}, LittleCMS says {wanted:?}"
                );
            }
        }
    }

    /// The same display with the Rec. 2020 transfer function, written as an
    /// ICC parametric curve — the form a real profile uses. Again LittleCMS's
    /// numbers.
    #[test]
    fn a_parametric_display_matches_lcms2() {
        let curve = para_type3(2.222_24, 0.909_9, 0.090_01, 0.222_22, 0.081_01);
        let transform =
            DisplayTransform::for_profile(&profile(&REC2020_D50, &curve)).expect("a transform");
        let expected = [
            ([1.0, 0.0, 0.0], [0.792_1, 0.231_3, 0.073_7]),
            ([0.0, 1.0, 0.0], [0.567_8, 0.959_4, 0.269_2]),
            ([0.0, 0.0, 1.0], [0.168_8, 0.051, 0.946_7]),
            (
                [0.501_960_8, 0.501_960_8, 0.501_960_8],
                [0.452_4, 0.452_4, 0.452_3],
            ),
            ([0.2, 0.4, 0.6], [0.250_4, 0.337, 0.537_9]),
        ];
        for (input, wanted) in expected {
            let mapped = transform.map(input);
            for channel in 0..3 {
                assert!(
                    (mapped[channel] - wanted[channel]).abs() < 3e-3,
                    "{input:?} became {mapped:?}, LittleCMS says {wanted:?}"
                );
            }
        }
    }

    /// The cube the renderer uploads: a corner at each end, alpha full, and the
    /// red channel varying fastest — the order the texture is sampled in.
    #[test]
    fn the_baked_cube_is_the_documented_shape() {
        let transform =
            DisplayTransform::for_profile(&profile(&SRGB_D50, &srgb_curve())).expect("a transform");
        let lut = transform.lut();
        assert_eq!(lut.len(), LUT_SIZE * LUT_SIZE * LUT_SIZE);
        assert!(lut[0][0] < 1e-3 && lut[0][1] < 1e-3 && lut[0][2] < 1e-3);
        assert!(lut.iter().all(|entry| entry[3] == 1.0));
        let last = lut[lut.len() - 1];
        assert!((last[0] - 1.0).abs() < 1e-3 && (last[1] - 1.0).abs() < 1e-3);
        // The second entry has red one step up and green and blue at zero.
        let step = 1.0 / (LUT_SIZE - 1) as f32;
        assert!((lut[1][0] - step).abs() < 1e-3, "{:?}", lut[1]);
        assert!(lut[1][1] < 1e-3 && lut[1][2] < 1e-3);
        // And the entry after a full row of red is green one step up.
        let green_step = lut[LUT_SIZE];
        assert!(green_step[0] < 1e-3 && (green_step[1] - step).abs() < 1e-3);
    }

    /// Profiles this cannot honestly transform are refused rather than guessed
    /// at: the screen then keeps whatever it had.
    #[test]
    fn a_profile_that_cannot_be_read_is_refused() {
        // No transfer curves at all — a matrix-only profile (the shape of the
        // Rec. 2020 test profiles in the grim tree).
        let mut tags: Vec<(&[u8; 4], Vec<u8>)> = vec![
            (b"wtpt", xyz_tag([0.9642, 1.0, 0.8249])),
            (
                b"rXYZ",
                xyz_tag([SRGB_D50[0][0], SRGB_D50[1][0], SRGB_D50[2][0]]),
            ),
            (
                b"gXYZ",
                xyz_tag([SRGB_D50[0][1], SRGB_D50[1][1], SRGB_D50[2][1]]),
            ),
            (
                b"bXYZ",
                xyz_tag([SRGB_D50[0][2], SRGB_D50[1][2], SRGB_D50[2][2]]),
            ),
        ];
        tags.sort_by(|a, b| a.0.cmp(b.0));
        let mut header = vec![0u8; 128];
        header[20..24].copy_from_slice(b"XYZ ");
        header[16..20].copy_from_slice(b"RGB ");
        header[36..40].copy_from_slice(b"acsp");
        let mut body = Vec::new();
        let mut table = Vec::new();
        let mut offset = 132 + 12 * tags.len();
        for (signature, data) in &tags {
            while !offset.is_multiple_of(4) {
                body.push(0);
                offset += 1;
            }
            table.extend_from_slice(*signature);
            table.extend_from_slice(&(offset as u32).to_be_bytes());
            table.extend_from_slice(&(data.len() as u32).to_be_bytes());
            body.extend_from_slice(data);
            offset += data.len();
        }
        let total = (128 + 4 + table.len() + body.len()) as u32;
        header[0..4].copy_from_slice(&total.to_be_bytes());
        let mut no_curves = header;
        no_curves.extend_from_slice(&(tags.len() as u32).to_be_bytes());
        no_curves.extend_from_slice(&table);
        no_curves.extend_from_slice(&body);
        assert!(DisplayTransform::for_profile(&no_curves).is_none());

        // A truncated file, a file that is not a profile, and a curve that
        // runs backwards.
        assert!(DisplayTransform::for_profile(b"not a profile").is_none());
        let mut truncated = profile(&SRGB_D50, &srgb_curve());
        truncated.truncate(200);
        assert!(DisplayTransform::for_profile(&truncated).is_none());
        let mut backwards = b"curv\0\0\0\0".to_vec();
        backwards.extend_from_slice(&3u32.to_be_bytes());
        for value in [40_000u16, 30_000, 50_000] {
            backwards.extend_from_slice(&value.to_be_bytes());
        }
        assert!(DisplayTransform::for_profile(&profile(&SRGB_D50, &backwards)).is_none());

        // A profile whose primaries have no inverse.
        let singular = [[0.0, 0.0, 0.0], [0.0, 0.0, 0.0], [0.0, 0.0, 0.0]];
        assert!(DisplayTransform::for_profile(&profile(&singular, &srgb_curve())).is_none());
    }
}

/// A display transform, baked and ready for the renderer.
///
/// The token names this cube to the renderer, which skips uploading one it has
/// already been given: a profile does not change between frames, and a
/// compositor drawing two outputs with different profiles alternates between
/// their cubes every frame.
pub struct Cube {
    pub token: u64,
    pub samples: std::sync::Arc<Vec<[f32; 4]>>,
}
