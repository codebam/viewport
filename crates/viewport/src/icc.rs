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

/// XYZ in a LUT is 1.15 fixed point — 0xFFFF is just under 2.0, so a PCS
/// component meets the table at `value × 32768/65535`, not at full scale.
const XYZ_PCS_SCALE: f64 = 32768.0 / 65535.0;

/// A clamp that does not pass NaN through: `f64::clamp` returns NaN for NaN,
/// and a cube entry the renderer samples as NaN is a black pixel, not a colour.
fn finite_clamp(value: f64) -> f64 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// What to send a monitor so that an sRGB picture looks like sRGB.
///
/// A matrix-shaper display profile is three primaries (rXYZ, gXYZ, bXYZ, as PCS
/// XYZ relative to D50) and three transfer curves (rTRC, gTRC, bTRC, device
/// value to linear light). Reading it backwards gives the transform the screen
/// needs: sRGB encoded → linear sRGB → PCS D50 → linear display → display
/// encoded. A LUT-described profile says the same thing as a table, and the
/// direction the screen needs is its `B2A`/`B2D` (PCS → device) tag — forward,
/// so nothing here inverts a table. Either way, `lut` is that function sampled
/// on a cube, which is the form the renderer can upload as a 3D texture.
///
/// The matrix description is tried first, so a profile that carries both keeps
/// the transform it shipped with; the LUT is what a profile with no primaries
/// and curves is read through. What is still refused: a profile whose only
/// tables run device → PCS (`A2B`), a Lab PCS, a non-RGB device space, and any
/// tag this cannot read whole — a profile that cannot be understood is not a
/// reason to guess a transform.
pub struct DisplayTransform {
    kind: Kind,
    /// sRGB encoded → PCS XYZ D50.
    srgb_to_pcs: Mat3,
    /// `r + g*N + b*N²` entries, each `[r, g, b, 1.0]`, display encoded.
    lut: Vec<[f32; 4]>,
}

/// The two ways a display profile can say what the panel does.
enum Kind {
    /// Three primaries and three transfer curves, read backwards.
    Matrix {
        /// display encoded → linear light, per channel.
        decode: [Curve; 3],
        /// PCS XYZ D50 → linear display RGB.
        from_pcs: Mat3,
    },
    /// A PCS → device table, read forwards.
    Lut(LutPipeline),
}

impl DisplayTransform {
    /// Read a display profile: matrix-shaper first, then a LUT. Anything else
    /// is `None`: a profile that cannot be understood is not a reason to guess
    /// a transform.
    pub fn for_profile(profile: &[u8]) -> Option<Self> {
        if profile.len() < 132 || &profile[36..40] != b"acsp" {
            return None;
        }
        // The transform is RGB in, RGB out; an XYZ or Lab device space is a
        // different question with a different answer.
        if &profile[16..20] != b"RGB " || &profile[20..24] != b"XYZ " {
            return None;
        }
        let kind = Self::matrix_kind(profile).or_else(|| Self::lut_kind(profile))?;
        let mut transform = Self {
            kind,
            srgb_to_pcs: multiply(&BRADFORD_D65_TO_D50, &SRGB_TO_XYZ_D65),
            lut: Vec::new(),
        };
        transform.lut = transform.bake();
        Some(transform)
    }

    fn matrix_kind(profile: &[u8]) -> Option<Kind> {
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
        Some(Kind::Matrix { decode, from_pcs })
    }

    fn lut_kind(profile: &[u8]) -> Option<Kind> {
        // The order LittleCMS reads an output profile in for relative
        // colorimetric: the v4 float tag first, then the 16-bit
        // relative-colorimetric tag, then — only when that tag is absent —
        // the perceptual one. A tag that is present but cannot be read is a
        // refusal, not an invitation to try the next: half a transform is
        // worse than none.
        let data = tag_data(profile, b"B2D1")
            .or_else(|| tag_data(profile, b"B2A1"))
            .or_else(|| tag_data(profile, b"B2A0"))?;
        Some(Kind::Lut(parse_lut_tag(data)?))
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
        match &self.kind {
            Kind::Matrix { decode, from_pcs } => {
                let device = mul_vec(from_pcs, &pcs);
                let mut out = [0.0; 3];
                for (channel, value) in out.iter_mut().enumerate() {
                    *value = decode[channel]
                        .invert(device[channel].clamp(0.0, 1.0))
                        .clamp(0.0, 1.0);
                }
                out
            }
            Kind::Lut(pipeline) => {
                // The table itself does the gamut mapping its intent
                // describes; the screen clips what is left.
                let scaled = [
                    pcs[0] * pipeline.pcs_scale,
                    pcs[1] * pipeline.pcs_scale,
                    pcs[2] * pipeline.pcs_scale,
                ];
                let device = pipeline.eval(scaled);
                [
                    finite_clamp(device[0]),
                    finite_clamp(device[1]),
                    finite_clamp(device[2]),
                ]
            }
        }
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
        Self::from_samples(curve_tag_samples(data)?.0)
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

/// A `curv` or `para` tag as forward samples, and the tag's byte length — the
/// length so a reader walking several embedded curves knows where the next one
/// starts. The matrix path's [`Curve`] adds the monotonicity an inverse needs;
/// a LUT's curves are only ever evaluated forwards, so they do not.
fn curve_tag_samples(data: &[u8]) -> Option<(Vec<f64>, usize)> {
    if data.len() < 12 {
        return None;
    }
    match &data[0..4] {
        b"curv" => {
            let count = be_u32(&data[8..12]) as usize;
            let (samples, size) = match count {
                // An empty table is the identity, by the specification.
                0 => (
                    (0..CURVE_SAMPLES)
                        .map(|i| i as f64 / (CURVE_SAMPLES - 1) as f64)
                        .collect(),
                    12,
                ),
                // A single entry is a gamma in 8.8 fixed point.
                1 => {
                    if data.len() < 14 {
                        return None;
                    }
                    let gamma = f64::from(be_u16(&data[12..14])) / 256.0;
                    if !(gamma.is_finite() && gamma > 0.0) {
                        return None;
                    }
                    (
                        (0..CURVE_SAMPLES)
                            .map(|i| (i as f64 / (CURVE_SAMPLES - 1) as f64).powf(gamma))
                            .collect(),
                        14,
                    )
                }
                count => {
                    let bytes = count.checked_mul(2)?;
                    if data.len() < 12 + bytes {
                        return None;
                    }
                    let samples = (0..count)
                        .map(|i| f64::from(be_u16(&data[12 + 2 * i..14 + 2 * i])) / 65535.0)
                        .collect();
                    (samples, 12 + bytes)
                }
            };
            Some((samples, size))
        }
        b"para" => {
            // type (u16), reserved (u16), then up to seven S15Fixed16 parameters.
            let kind = be_u16(&data[8..10]);
            let count = match kind {
                0 => 1,
                1 => 3,
                2 => 4,
                3 => 5,
                4 => 7,
                _ => return None,
            };
            let size = 12 + count * 4;
            if data.len() < size {
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
            Some((samples, size))
        }
        _ => None,
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

// ---------------------------------------------------------------------------
// LUT-described profiles
// ---------------------------------------------------------------------------
//
// A matrix-shaper profile is not the only way to say what a display does. A
// LUT profile stores the transform as tables: per-channel curves, a 3D CLUT
// that mixes the channels, and sometimes a matrix. The direction a screen
// needs — PCS to device — is the `B2A` (v2 and v4) or `B2D` (v4 float) tag
// family, and it is read forwards, so unlike the matrix path nothing here
// inverts anything. The four tag types this reads (`mft1`, `mft2`, `mBA `,
// `mpet`) all decompose into the same three stage kinds, so one evaluator
// serves them all; only the parsing differs.
//
// The reference for every layout and every evaluation rule below is LittleCMS
// 2.19, the same engine the matrix path is cross-checked against: the stage
// order, the tetrahedral CLUT interpolation, the 1.15 fixed-point PCS, and the
// segmented-curve formulas are its behaviour, read out of its source rather
// than guessed from the specification's prose.

/// The most entries one CLUT may hold. 255 points per dimension is legal ICC
/// and would be half a gigabyte of f64; real display LUTs are 17³ to 65³, and
/// this cap (about 87³ uniform) leaves every real profile room while a corrupt
/// grid cannot allocate without bound.
const CLUT_ENTRY_CAP: usize = 1 << 21;

/// The most segments one segmented curve may carry, and the most samples across
/// them — a real curve has one to three segments; these bound a corrupt count.
const SEGMENT_CAP: usize = 1024;
const SEGMENT_SAMPLE_CAP: usize = 1 << 16;

/// The sentinels LittleCMS uses for the open ends of a segmented curve's
/// domain. They are finite float32 values, not infinities, and the sampled
/// interpolation below divides in float32 exactly as it does — a first
/// segment that is sampled rather than formula is degenerate there (its whole
/// domain reads as one sample), and matching that is matching the engine.
const SEG_MINUS_INF: f32 = -1e22;
const SEG_PLUS_INF: f32 = 1e22;

/// A forward curve: encoded in, encoded out, no inverse asked of it. Unlike
/// [`Curve`] it need not be monotone — a LUT's curves may bend backwards where
/// the gamut mapping needs them to.
enum Table {
    /// Samples evenly spaced over [0, 1].
    Uniform(Vec<f64>),
    /// A v4 segmented curve (`curf`): pieces over breakpoint-delimited ranges.
    Segmented(Vec<Segment>),
}

struct Segment {
    x0: f32,
    x1: f32,
    kind: SegmentKind,
}

enum SegmentKind {
    /// Samples evenly spaced over [x0, x1]; the first is the curve's value at
    /// x0, implicit in the file and filled from the preceding segment.
    Sampled(Vec<f64>),
    /// One of the three segmented formula types, evaluated exactly.
    Formula { kind: u16, params: Vec<f64> },
}

impl Table {
    fn uniform(samples: Vec<f64>) -> Option<Self> {
        if samples.len() < 2 || samples.iter().any(|value| !value.is_finite()) {
            return None;
        }
        Some(Self::Uniform(samples))
    }

    fn eval(&self, x: f64) -> f64 {
        match self {
            Self::Uniform(samples) => {
                let at = x.clamp(0.0, 1.0) * (samples.len() - 1) as f64;
                let index = (at.floor() as usize).min(samples.len() - 2);
                let fraction = at - index as f64;
                samples[index] * (1.0 - fraction) + samples[index + 1] * fraction
            }
            Self::Segmented(segments) => {
                // LittleCMS walks the segments backwards and takes the first
                // whose range is (x0, x1] — a breakpoint belongs to the piece
                // below it.
                for segment in segments.iter().rev() {
                    if x > f64::from(segment.x0) && x <= f64::from(segment.x1) {
                        return match &segment.kind {
                            SegmentKind::Sampled(samples) => {
                                let span = segment.x1 - segment.x0;
                                let at = ((x - f64::from(segment.x0)) as f32) / span;
                                let at = f64::from(at.clamp(0.0, 1.0)) * (samples.len() - 1) as f64;
                                let index = (at.floor() as usize).min(samples.len() - 2);
                                let fraction = at - index as f64;
                                samples[index] * (1.0 - fraction) + samples[index + 1] * fraction
                            }
                            SegmentKind::Formula { kind, params } => {
                                segment_formula(*kind, params, x)
                            }
                        };
                    }
                }
                f64::from(SEG_MINUS_INF)
            }
        }
    }
}

/// The three segmented-curve formula types, as LittleCMS evaluates them (its
/// internal types 6, 7 and 8; the file stores 0, 1 and 2).
fn segment_formula(kind: u16, p: &[f64], x: f64) -> f64 {
    match kind {
        // Y = (a·X + b)^g + c, with the gamma-1 and negative-base special cases.
        6 => {
            let e = p[1] * x + p[2];
            if p[0] == 1.0 {
                e + p[3]
            } else if e < 0.0 {
                p[3]
            } else {
                e.powf(p[0]) + p[3]
            }
        }
        // Y = a·log10(b·X^g + c) + d.
        7 => {
            let e = p[2] * x.powf(p[0]) + p[3];
            if e <= 0.0 {
                p[4]
            } else {
                p[1] * e.log10() + p[4]
            }
        }
        // Y = a·b^(c·X + d) + e.
        8 => p[0] * p[1].powf(p[2] * x + p[3]) + p[4],
        _ => 0.0,
    }
}

/// A 3-in 3-out CLUT. The table is laid out the way ICC lays it out — the first
/// input channel slowest, the last fastest, three output channels per node —
/// which is the opposite end of the cube from the renderer's texture order;
/// the bake reconciles them by sampling [`DisplayTransform::map`].
struct Clut {
    grid: [usize; 3],
    table: Vec<f64>,
}

impl Clut {
    fn new(grid: [usize; 3], table: Vec<f64>) -> Option<Self> {
        if grid.iter().any(|&points| points < 2) {
            return None;
        }
        let entries = grid[0]
            .checked_mul(grid[1])?
            .checked_mul(grid[2])?
            .checked_mul(3)?;
        if entries > CLUT_ENTRY_CAP || table.len() != entries {
            return None;
        }
        if table.iter().any(|value| !value.is_finite()) {
            return None;
        }
        Some(Self { grid, table })
    }

    /// Tetrahedral interpolation (Sakamoto), the default LittleCMS uses for a
    /// 3D CLUT — trilinear is only its Lab-PCS fallback, and this pipeline is
    /// XYZ. The six tetrahedra partition the cell by the ordering of the three
    /// fractional coordinates; within each, the answer is affine, so the four
    /// node differences below reproduce it exactly.
    fn eval(&self, input: [f64; 3]) -> [f64; 3] {
        let domain = [
            (self.grid[0] - 1) as f64,
            (self.grid[1] - 1) as f64,
            (self.grid[2] - 1) as f64,
        ];
        let clamped = [
            input[0].clamp(0.0, 1.0),
            input[1].clamp(0.0, 1.0),
            input[2].clamp(0.0, 1.0),
        ];
        let scaled = [
            clamped[0] * domain[0],
            clamped[1] * domain[1],
            clamped[2] * domain[2],
        ];
        let base = [
            scaled[0].floor() as usize,
            scaled[1].floor() as usize,
            scaled[2].floor() as usize,
        ];
        let frac = [
            scaled[0] - base[0] as f64,
            scaled[1] - base[1] as f64,
            scaled[2] - base[2] as f64,
        ];
        let stride = [self.grid[1] * self.grid[2] * 3, self.grid[2] * 3, 3];
        // At exactly 1.0 the floor is the last node and there is no node above
        // it; the fraction is zero, so the upper corner is never read.
        let lo = [
            base[0] * stride[0],
            base[1] * stride[1],
            base[2] * stride[2],
        ];
        let hi = [
            lo[0] + if clamped[0] >= 1.0 { 0 } else { stride[0] },
            lo[1] + if clamped[1] >= 1.0 { 0 } else { stride[1] },
            lo[2] + if clamped[2] >= 1.0 { 0 } else { stride[2] },
        ];
        let node = |x: usize, y: usize, z: usize, channel: usize| self.table[x + y + z + channel];
        let (rx, ry, rz) = (frac[0], frac[1], frac[2]);
        let mut out = [0.0; 3];
        for (channel, out_value) in out.iter_mut().enumerate() {
            let c0 = node(lo[0], lo[1], lo[2], channel);
            let (c1, c2, c3) = if rx >= ry && ry >= rz {
                (
                    node(hi[0], lo[1], lo[2], channel) - c0,
                    node(hi[0], hi[1], lo[2], channel) - node(hi[0], lo[1], lo[2], channel),
                    node(hi[0], hi[1], hi[2], channel) - node(hi[0], hi[1], lo[2], channel),
                )
            } else if rx >= rz && rz >= ry {
                (
                    node(hi[0], lo[1], lo[2], channel) - c0,
                    node(hi[0], hi[1], hi[2], channel) - node(hi[0], lo[1], hi[2], channel),
                    node(hi[0], lo[1], hi[2], channel) - node(hi[0], lo[1], lo[2], channel),
                )
            } else if rz >= rx && rx >= ry {
                (
                    node(hi[0], lo[1], hi[2], channel) - node(lo[0], lo[1], hi[2], channel),
                    node(hi[0], hi[1], hi[2], channel) - node(hi[0], lo[1], hi[2], channel),
                    node(lo[0], lo[1], hi[2], channel) - c0,
                )
            } else if ry >= rx && rx >= rz {
                (
                    node(hi[0], hi[1], lo[2], channel) - node(lo[0], hi[1], lo[2], channel),
                    node(lo[0], hi[1], lo[2], channel) - c0,
                    node(hi[0], hi[1], hi[2], channel) - node(hi[0], hi[1], lo[2], channel),
                )
            } else if ry >= rz && rz >= rx {
                (
                    node(hi[0], hi[1], hi[2], channel) - node(lo[0], hi[1], hi[2], channel),
                    node(lo[0], hi[1], lo[2], channel) - c0,
                    node(lo[0], hi[1], hi[2], channel) - node(lo[0], hi[1], lo[2], channel),
                )
            } else if rz >= ry && ry >= rx {
                (
                    node(hi[0], hi[1], hi[2], channel) - node(lo[0], hi[1], hi[2], channel),
                    node(lo[0], hi[1], hi[2], channel) - node(lo[0], lo[1], hi[2], channel),
                    node(lo[0], lo[1], hi[2], channel) - c0,
                )
            } else {
                (0.0, 0.0, 0.0)
            };
            *out_value = c0 + c1 * rx + c2 * ry + c3 * rz;
        }
        out
    }
}

/// One processing element of a LUT. Every stage is 3-in 3-out: the profile is
/// RGB and the PCS is XYZ, and anything else is refused at parse time.
enum Stage {
    Curves([Table; 3]),
    Matrix(Mat3, [f64; 3]),
    Clut(Clut),
}

/// A PCS → device transform as an ordered list of stages.
struct LutPipeline {
    stages: Vec<Stage>,
    /// True for the v4 float (MPE) types, which carry full-precision values
    /// between stages; the 16-bit types quantize at every stage boundary, so
    /// their values are clamped there the way a u16 pipeline clamps them.
    float: bool,
    /// What the PCS is multiplied by before it meets the first stage. The
    /// 16-bit tags index it in 1.15 fixed point (×32768/65535); the float tag
    /// meets it at full scale — LittleCMS normalizes the float PCS back to
    /// XYZ before the tag's own pipeline runs.
    pcs_scale: f64,
}

impl LutPipeline {
    fn eval(&self, input: [f64; 3]) -> [f64; 3] {
        let mut value = input;
        for stage in &self.stages {
            value = match stage {
                Stage::Curves(curves) => [
                    curves[0].eval(value[0]),
                    curves[1].eval(value[1]),
                    curves[2].eval(value[2]),
                ],
                Stage::Matrix(matrix, offset) => {
                    let mixed = mul_vec(matrix, &value);
                    [
                        mixed[0] + offset[0],
                        mixed[1] + offset[1],
                        mixed[2] + offset[2],
                    ]
                }
                Stage::Clut(clut) => clut.eval(value),
            };
            if !self.float {
                value = [
                    finite_clamp(value[0]),
                    finite_clamp(value[1]),
                    finite_clamp(value[2]),
                ];
            }
        }
        value
    }
}

/// Read one PCS → device tag, whichever of the four LUT types it is.
fn parse_lut_tag(data: &[u8]) -> Option<LutPipeline> {
    if data.len() < 8 {
        return None;
    }
    match &data[0..4] {
        b"mft1" => parse_lut_table(data, false),
        b"mft2" => parse_lut_table(data, true),
        b"mBA " => parse_mba(data),
        // The MPE type is direction-agnostic — the tag it sits in says which
        // way it runs, and the element order in its position table is the
        // order to evaluate.
        b"mpet" => parse_mpe(data),
        _ => None,
    }
}

/// `mft1` (8-bit) and `mft2` (16-bit): an optional inline matrix, input tables,
/// an optional CLUT, output tables. The matrix comes first — LittleCMS inserts
/// it at the head of the pipeline, ahead of the input tables.
fn parse_lut_table(data: &[u8], wide: bool) -> Option<LutPipeline> {
    if data.len() < 12 {
        return None;
    }
    let (input, output, grid) = (
        usize::from(data[8]),
        usize::from(data[9]),
        usize::from(data[10]),
    );
    if input != 3 || output != 3 || grid == 1 {
        return None;
    }
    let mut at = 12;
    let matrix = read_fixed_matrix(data, at)?;
    at += 36;
    let mut stages = Vec::new();
    if !matrix_is_identity(&matrix) {
        stages.push(Stage::Matrix(matrix, [0.0; 3]));
    }
    let (in_entries, out_entries) = if wide {
        if data.len() < at + 4 {
            return None;
        }
        let in_entries = usize::from(be_u16(&data[at..at + 2]));
        let out_entries = usize::from(be_u16(&data[at + 2..at + 4]));
        at += 4;
        if in_entries > 0x7FFF || out_entries > 0x7FFF {
            return None;
        }
        (in_entries, out_entries)
    } else {
        (256, 256)
    };
    if let Some(curves) = read_raw_tables(data, &mut at, in_entries, wide)? {
        stages.push(Stage::Curves(curves));
    }
    if grid > 0 {
        let entries = grid.checked_mul(grid)?.checked_mul(grid)?.checked_mul(3)?;
        if entries > CLUT_ENTRY_CAP {
            return None;
        }
        let table = read_normalized_entries(data, &mut at, entries, wide)?;
        stages.push(Stage::Clut(Clut::new([grid; 3], table)?));
    }
    if let Some(curves) = read_raw_tables(data, &mut at, out_entries, wide)? {
        stages.push(Stage::Curves(curves));
    }
    if stages.is_empty() {
        return None;
    }
    Some(LutPipeline {
        stages,
        float: false,
        pcs_scale: XYZ_PCS_SCALE,
    })
}

/// Three per-channel tables of `entries` samples each. Zero entries is an
/// absent table (a LittleCMS extension the spec's writers rely on); one entry
/// is a corrupt table, not a gamma — that spelling belongs to `curv`.
fn read_raw_tables(
    data: &[u8],
    at: &mut usize,
    entries: usize,
    wide: bool,
) -> Option<Option<[Table; 3]>> {
    if entries == 0 {
        return Some(None);
    }
    if entries < 2 {
        return None;
    }
    let mut curves = Vec::with_capacity(3);
    for _ in 0..3 {
        let samples = read_normalized_entries(data, at, entries, wide)?;
        curves.push(Table::uniform(samples)?);
    }
    Some(Some([curves.remove(0), curves.remove(0), curves.remove(0)]))
}

/// `entries` samples, u8 or u16 big-endian, normalized to [0, 1].
fn read_normalized_entries(
    data: &[u8],
    at: &mut usize,
    entries: usize,
    wide: bool,
) -> Option<Vec<f64>> {
    let size = if wide { 2 } else { 1 };
    let bytes = entries.checked_mul(size)?;
    let end = at.checked_add(bytes)?;
    if end > data.len() {
        return None;
    }
    let samples = (0..entries)
        .map(|i| {
            if wide {
                f64::from(be_u16(&data[*at + 2 * i..*at + 2 * i + 2])) / 65535.0
            } else {
                f64::from(data[*at + i]) / 255.0
            }
        })
        .collect();
    *at = end;
    Some(samples)
}

/// `mBA ` (v4): B curves, an optional matrix, optional M curves, an optional
/// CLUT, optional A curves — in that order, each at its own offset from the
/// tag's start, zero meaning absent. The allowed combinations are B alone,
/// B–matrix–M, B–CLUT–A and B–matrix–M–CLUT–A; reading the offsets in order
/// reproduces them without enumerating them.
fn parse_mba(data: &[u8]) -> Option<LutPipeline> {
    if data.len() < 32 {
        return None;
    }
    if data[8] != 3 || data[9] != 3 {
        return None;
    }
    let offset = |at: usize| usize::try_from(be_u32(&data[at..at + 4])).ok();
    let (off_b, off_mat, off_m, off_c, off_a) = (
        offset(12)?,
        offset(16)?,
        offset(20)?,
        offset(24)?,
        offset(28)?,
    );
    let mut stages = Vec::new();
    if off_b != 0 {
        stages.push(Stage::Curves(read_embedded_curves(data, off_b)?));
    }
    if off_mat != 0 {
        stages.push(read_matrix_element(data, off_mat)?);
    }
    if off_m != 0 {
        stages.push(Stage::Curves(read_embedded_curves(data, off_m)?));
    }
    if off_c != 0 {
        stages.push(Stage::Clut(read_mba_clut(data, off_c)?));
    }
    if off_a != 0 {
        stages.push(Stage::Curves(read_embedded_curves(data, off_a)?));
    }
    if stages.is_empty() {
        return None;
    }
    Some(LutPipeline {
        stages,
        float: false,
        pcs_scale: XYZ_PCS_SCALE,
    })
}

/// Three `curv`/`para` curves laid out one after another, each aligned to four
/// bytes — the way `mBA ` stores its curve sets.
fn read_embedded_curves(data: &[u8], offset: usize) -> Option<[Table; 3]> {
    let mut at = offset;
    let mut curves = Vec::with_capacity(3);
    for _ in 0..3 {
        let (samples, size) = curve_tag_samples(data.get(at..)?)?;
        curves.push(Table::uniform(samples)?);
        at = at.checked_add(size)?;
        at = (at + 3) & !3;
    }
    Some([curves.remove(0), curves.remove(0), curves.remove(0)])
}

/// A matrix element: nine S15Fixed16 coefficients then three offsets.
fn read_matrix_element(data: &[u8], offset: usize) -> Option<Stage> {
    let end = offset.checked_add(48)?;
    if end > data.len() {
        return None;
    }
    let fixed = |at: usize| f64::from(be_i32(&data[at..at + 4])) / 65536.0;
    let matrix = [
        [fixed(offset), fixed(offset + 4), fixed(offset + 8)],
        [fixed(offset + 12), fixed(offset + 16), fixed(offset + 20)],
        [fixed(offset + 24), fixed(offset + 28), fixed(offset + 32)],
    ];
    let offset_vec = [fixed(offset + 36), fixed(offset + 40), fixed(offset + 44)];
    if !matrix.iter().flatten().all(|v| v.is_finite()) || !offset_vec.iter().all(|v| v.is_finite())
    {
        return None;
    }
    Some(Stage::Matrix(matrix, offset_vec))
}

/// The inline matrix of a `mft1`/`mft2` tag: nine S15Fixed16, no offsets.
fn read_fixed_matrix(data: &[u8], at: usize) -> Option<Mat3> {
    if at + 36 > data.len() {
        return None;
    }
    let fixed = |i: usize| f64::from(be_i32(&data[at + i..at + i + 4])) / 65536.0;
    let matrix = [
        [fixed(0), fixed(4), fixed(8)],
        [fixed(12), fixed(16), fixed(20)],
        [fixed(24), fixed(28), fixed(32)],
    ];
    if !matrix.iter().flatten().all(|v| v.is_finite()) {
        return None;
    }
    Some(matrix)
}

fn matrix_is_identity(matrix: &Mat3) -> bool {
    const IDENTITY: Mat3 = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    matrix == &IDENTITY
}

/// The CLUT of a `mBA ` tag: sixteen grid-point bytes (one per possible
/// dimension, three used), a precision byte, three padding bytes, then the
/// nodes. A grid point of 1 is impossible anywhere in the sixteen.
fn read_mba_clut(data: &[u8], offset: usize) -> Option<Clut> {
    let header_end = offset.checked_add(20)?;
    if header_end > data.len() {
        return None;
    }
    let grid = clut_grid(&data[offset..offset + 16])?;
    let precision = data[offset + 16];
    let wide = match precision {
        1 => false,
        2 => true,
        _ => return None,
    };
    let entries = grid[0]
        .checked_mul(grid[1])?
        .checked_mul(grid[2])?
        .checked_mul(3)?;
    if entries > CLUT_ENTRY_CAP {
        return None;
    }
    let mut at = header_end;
    let table = read_normalized_entries(data, &mut at, entries, wide)?;
    Clut::new(grid, table)
}

/// The first three of the sixteen grid-point bytes, validated: none of the
/// sixteen may be 1, and the three used must be at least 2.
fn clut_grid(bytes: &[u8]) -> Option<[usize; 3]> {
    if bytes.len() < 16 || bytes.contains(&1) {
        return None;
    }
    let grid = [
        usize::from(bytes[0]),
        usize::from(bytes[1]),
        usize::from(bytes[2]),
    ];
    if grid.iter().any(|&points| points < 2) {
        return None;
    }
    Some(grid)
}

/// `mpet` (v4 float): a position table of elements, each a curve set, a
/// matrix or a float CLUT, evaluated in the order the table gives. The two
/// ACS element signatures are skipped, as the specification directs.
fn parse_mpe(data: &[u8]) -> Option<LutPipeline> {
    if data.len() < 16 {
        return None;
    }
    if be_u16(&data[8..10]) != 3 || be_u16(&data[10..12]) != 3 {
        return None;
    }
    let count = be_u32(&data[12..16]) as usize;
    if count == 0 || count > 256 {
        return None;
    }
    let table_at = 16;
    if data.len() < table_at + count * 8 {
        return None;
    }
    let mut stages = Vec::new();
    for i in 0..count {
        let entry = table_at + i * 8;
        let offset = be_u32(&data[entry..entry + 4]) as usize;
        let size = be_u32(&data[entry + 4..entry + 8]) as usize;
        let end = offset.checked_add(size)?;
        if offset < table_at || size < 8 || end > data.len() {
            return None;
        }
        let element = &data[offset..end];
        match &element[0..4] {
            b"cvst" => stages.push(Stage::Curves(parse_mpe_curves(element)?)),
            b"matf" => stages.push(parse_mpe_matrix(element)?),
            b"clut" => stages.push(Stage::Clut(parse_mpe_clut(element)?)),
            b"bACS" | b"eACS" => {}
            _ => return None,
        }
    }
    if stages.is_empty() {
        return None;
    }
    Some(LutPipeline {
        stages,
        float: true,
        pcs_scale: 1.0,
    })
}

/// A `cvst` element: three segmented curves at their own offsets. The element
/// opens with its signature and a reserved word, like every MPE element; the
/// channel counts follow, and the curve offsets are from the element's start.
fn parse_mpe_curves(element: &[u8]) -> Option<[Table; 3]> {
    if element.len() < 8 + 4 + 24 {
        return None;
    }
    if be_u16(&element[8..10]) != 3 || be_u16(&element[10..12]) != 3 {
        return None;
    }
    let mut curves = Vec::with_capacity(3);
    for i in 0..3 {
        let entry = 12 + i * 8;
        let offset = be_u32(&element[entry..entry + 4]) as usize;
        let size = be_u32(&element[entry + 4..entry + 8]) as usize;
        let end = offset.checked_add(size)?;
        if end > element.len() {
            return None;
        }
        curves.push(segmented_table(&element[offset..end])?);
    }
    Some([curves.remove(0), curves.remove(0), curves.remove(0)])
}

/// A `matf` element: nine float32 coefficients then three float32 offsets.
fn parse_mpe_matrix(element: &[u8]) -> Option<Stage> {
    if element.len() < 8 + 4 + 48 {
        return None;
    }
    if be_u16(&element[8..10]) != 3 || be_u16(&element[10..12]) != 3 {
        return None;
    }
    let matrix = [
        [
            be_f32(&element[12..16]),
            be_f32(&element[16..20]),
            be_f32(&element[20..24]),
        ],
        [
            be_f32(&element[24..28]),
            be_f32(&element[28..32]),
            be_f32(&element[32..36]),
        ],
        [
            be_f32(&element[36..40]),
            be_f32(&element[40..44]),
            be_f32(&element[44..48]),
        ],
    ];
    let offset = [
        be_f32(&element[48..52]),
        be_f32(&element[52..56]),
        be_f32(&element[56..60]),
    ];
    if !matrix.iter().flatten().all(|v| v.is_finite()) || !offset.iter().all(|v| v.is_finite()) {
        return None;
    }
    Some(Stage::Matrix(matrix, offset))
}

/// A `clut` element: sixteen grid-point bytes, then float32 nodes.
fn parse_mpe_clut(element: &[u8]) -> Option<Clut> {
    if element.len() < 8 + 4 + 16 {
        return None;
    }
    if be_u16(&element[8..10]) != 3 || be_u16(&element[10..12]) != 3 {
        return None;
    }
    let grid = clut_grid(&element[12..28])?;
    let entries = grid[0]
        .checked_mul(grid[1])?
        .checked_mul(grid[2])?
        .checked_mul(3)?;
    if entries > CLUT_ENTRY_CAP || element.len() < 28 + entries * 4 {
        return None;
    }
    let table = (0..entries)
        .map(|i| be_f32(&element[28 + i * 4..32 + i * 4]))
        .collect();
    Clut::new(grid, table)
}

/// A segmented curve (`curf`): breakpoints, then formula or sampled pieces.
/// The first piece's lower bound and the last's upper bound are the open-end
/// sentinels; a sampled piece's first value is implicit — the curve's value at
/// the piece's lower breakpoint, which is the preceding piece's value there.
fn segmented_table(data: &[u8]) -> Option<Table> {
    if data.len() < 12 || &data[0..4] != b"curf" {
        return None;
    }
    let count = usize::from(be_u16(&data[8..10]));
    if count == 0 || count > SEGMENT_CAP {
        return None;
    }
    let break_bytes = (count - 1).checked_mul(4)?;
    let mut at = 12 + break_bytes;
    if data.len() < at {
        return None;
    }
    let mut breaks = Vec::with_capacity(count - 1);
    for i in 0..count - 1 {
        let value = be_f32(&data[12 + i * 4..16 + i * 4]);
        if !value.is_finite() || breaks.last().is_some_and(|&prev| value <= prev) {
            return None;
        }
        breaks.push(value);
    }
    let mut segments = Vec::with_capacity(count);
    let mut total_samples = 0usize;
    let mut x0 = SEG_MINUS_INF;
    // One upper bound per piece: the breakpoints, then the open end.
    let bounds = breaks
        .iter()
        .map(|&b| b as f32)
        .chain(std::iter::once(SEG_PLUS_INF));
    for x1 in bounds {
        if data.len() < at + 8 {
            return None;
        }
        let signature = &data[at..at + 4];
        at += 8;
        let kind = match signature {
            b"parf" => {
                if data.len() < at + 4 {
                    return None;
                }
                let kind = be_u16(&data[at..at + 2]);
                at += 4;
                let params = match kind {
                    0 => 4,
                    1 | 2 => 5,
                    _ => return None,
                };
                if data.len() < at + params * 4 {
                    return None;
                }
                let values: Vec<f64> = (0..params)
                    .map(|j| be_f32(&data[at + j * 4..at + j * 4 + 4]))
                    .collect();
                at += params * 4;
                if !values.iter().all(|v| v.is_finite()) {
                    return None;
                }
                SegmentKind::Formula {
                    kind: kind + 6,
                    params: values,
                }
            }
            b"samf" => {
                if data.len() < at + 4 {
                    return None;
                }
                let samples = be_u32(&data[at..at + 4]) as usize;
                at += 4;
                if samples == 0 || data.len() < at + samples * 4 {
                    return None;
                }
                total_samples += samples + 1;
                if total_samples > SEGMENT_SAMPLE_CAP {
                    return None;
                }
                let mut values = Vec::with_capacity(samples + 1);
                // The implicit first point, filled once every piece is read.
                values.push(0.0);
                for j in 0..samples {
                    let value = be_f32(&data[at + j * 4..at + j * 4 + 4]);
                    if !value.is_finite() {
                        return None;
                    }
                    values.push(value);
                }
                at += samples * 4;
                SegmentKind::Sampled(values)
            }
            _ => return None,
        };
        segments.push(Segment { x0, x1, kind });
        x0 = x1;
    }
    let mut table = Table::Segmented(segments);
    // Fill each sampled piece's implicit first point from the curve as it
    // stands: the value at x0 comes from the piece below, already filled.
    let mut firsts = Vec::with_capacity(count);
    for i in 0..count {
        if let Table::Segmented(segments) = &table {
            firsts.push((i, table.eval(f64::from(segments[i].x0))));
        }
    }
    if let Table::Segmented(segments) = &mut table {
        for (i, first) in firsts {
            if let SegmentKind::Sampled(values) = &mut segments[i].kind {
                values[0] = first;
            }
        }
    }
    Some(table)
}

/// An ICC float32: big-endian IEEE 754, widened to f64 for the arithmetic.
fn be_f32(bytes: &[u8]) -> f64 {
    f64::from(f32::from_bits(u32::from_be_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3],
    ])))
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
    pub(super) const SRGB_D50: Mat3 = [
        [0.436_074_7, 0.385_064_9, 0.143_080_4],
        [0.222_504_5, 0.716_878_6, 0.060_616_9],
        [0.013_932_2, 0.097_104_5, 0.714_173_3],
    ];

    /// Rec. 2020's primaries adapted to D50.
    pub(super) const REC2020_D50: Mat3 = [
        [0.673_478, 0.165_671, 0.125_064],
        [0.279_045, 0.675_343, 0.045_634],
        [-0.001_920, 0.029_995, 0.797_121],
    ];

    pub(super) fn fixed(value: f64) -> [u8; 4] {
        ((value * 65536.0).round() as i32).to_be_bytes()
    }

    pub(super) fn xyz_tag(value: [f64; 3]) -> Vec<u8> {
        let mut data = b"XYZ \0\0\0\0".to_vec();
        for component in value {
            data.extend_from_slice(&fixed(component));
        }
        data
    }

    pub(super) fn curv_gamma(gamma: f64) -> Vec<u8> {
        let mut data = b"curv\0\0\0\0".to_vec();
        data.extend_from_slice(&1u32.to_be_bytes());
        data.extend_from_slice(&((gamma * 256.0).round() as u16).to_be_bytes());
        data
    }

    pub(super) fn para_type3(g: f64, a: f64, b: f64, c: f64, d: f64) -> Vec<u8> {
        let mut data = b"para\0\0\0\0".to_vec();
        data.extend_from_slice(&3u16.to_be_bytes());
        data.extend_from_slice(&0u16.to_be_bytes());
        for parameter in [g, a, b, c, d] {
            data.extend_from_slice(&fixed(parameter));
        }
        data
    }

    /// The sRGB transfer function as an ICC parametric curve, type 3.
    pub(super) fn srgb_curve() -> Vec<u8> {
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

#[cfg(test)]
mod lut_tests {
    use super::transform_tests::{curv_gamma, fixed, srgb_curve, xyz_tag, SRGB_D50};
    use super::*;

    /// A profile with exactly the tags given, laid out the way a real one is.
    fn profile_with_tags(tags: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
        let mut tags = tags.to_vec();
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

    const IDENTITY: Mat3 = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

    /// The CLUT nodes of a grid, sampled from a function of the normalized
    /// input, in ICC order: first channel slowest, three outputs per node.
    fn clut_nodes(grid: usize, f: impl Fn([f64; 3]) -> [f64; 3]) -> Vec<f64> {
        let step = 1.0 / (grid - 1) as f64;
        let mut nodes = Vec::with_capacity(grid * grid * grid * 3);
        for i in 0..grid {
            for j in 0..grid {
                for k in 0..grid {
                    let out = f([i as f64 * step, j as f64 * step, k as f64 * step]);
                    nodes.extend_from_slice(&out);
                }
            }
        }
        nodes
    }

    fn words16(values: &[f64]) -> Vec<u8> {
        values
            .iter()
            .flat_map(|v| ((v.clamp(0.0, 1.0) * 65535.0).round() as u16).to_be_bytes())
            .collect()
    }

    /// A gamma encoding table: linear light in, encoded out.
    fn gamma_table(entries: usize, gamma: f64) -> Vec<f64> {
        (0..entries)
            .map(|i| (i as f64 / (entries - 1) as f64).powf(1.0 / gamma))
            .collect()
    }

    fn identity_table(entries: usize) -> Vec<f64> {
        (0..entries)
            .map(|i| i as f64 / (entries - 1) as f64)
            .collect()
    }

    /// `mft2`: matrix, input tables, CLUT, output tables.
    fn lut16_tag(
        matrix: &Mat3,
        input: &[Vec<f64>; 3],
        grid: usize,
        clut: &[f64],
        output: &[Vec<f64>; 3],
    ) -> Vec<u8> {
        let mut tag = b"mft2\0\0\0\0".to_vec();
        tag.extend_from_slice(&[3, 3, grid as u8, 0]);
        for row in matrix {
            for value in row {
                tag.extend_from_slice(&fixed(*value));
            }
        }
        tag.extend_from_slice(&(input[0].len() as u16).to_be_bytes());
        tag.extend_from_slice(&(output[0].len() as u16).to_be_bytes());
        for channel in input {
            tag.extend_from_slice(&words16(channel));
        }
        tag.extend_from_slice(&words16(clut));
        for channel in output {
            tag.extend_from_slice(&words16(channel));
        }
        tag
    }

    /// Three `curv`/`para` tags back to back, four-byte aligned — the way
    /// `mBA ` stores a curve set. The padding follows the last curve too:
    /// LittleCMS aligns after every curve it reads, the last included, and a
    /// set that ends unaligned is a read past the tag's end.
    fn curves_blob(curves: &[Vec<u8>; 3]) -> Vec<u8> {
        let mut blob = Vec::new();
        for curve in curves {
            blob.extend_from_slice(curve);
            while !blob.len().is_multiple_of(4) {
                blob.push(0);
            }
        }
        blob
    }

    fn matrix_element_bytes(matrix: &Mat3, offset: &[f64; 3]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for row in matrix {
            for value in row {
                bytes.extend_from_slice(&fixed(*value));
            }
        }
        for value in offset {
            bytes.extend_from_slice(&fixed(*value));
        }
        bytes
    }

    fn mba_clut_bytes(grid: [u8; 3], nodes: &[f64]) -> Vec<u8> {
        let mut bytes = vec![0u8; 16];
        bytes[0..3].copy_from_slice(&grid);
        bytes.push(2);
        bytes.extend_from_slice(&[0, 0, 0]);
        bytes.extend_from_slice(&words16(nodes));
        bytes
    }

    /// `mBA `: the five optional elements at their offsets, zero for absent.
    fn mba_tag(
        b: Option<Vec<u8>>,
        matrix: Option<Vec<u8>>,
        m: Option<Vec<u8>>,
        clut: Option<Vec<u8>>,
        a: Option<Vec<u8>>,
    ) -> Vec<u8> {
        let mut tag = b"mBA \0\0\0\0".to_vec();
        tag.extend_from_slice(&[3, 3]);
        tag.extend_from_slice(&0u16.to_be_bytes());
        let mut offsets = [0u32; 5];
        let mut body = Vec::new();
        let mut at: usize = 32;
        for (i, element) in [b, matrix, m, clut, a].into_iter().enumerate() {
            if let Some(bytes) = element {
                while !at.is_multiple_of(4) {
                    body.push(0);
                    at += 1;
                }
                offsets[i] = at as u32;
                body.extend_from_slice(&bytes);
                at += bytes.len();
            }
        }
        for offset in offsets {
            tag.extend_from_slice(&offset.to_be_bytes());
        }
        tag.extend_from_slice(&body);
        tag
    }

    /// A single-formula segmented curve over the whole domain.
    fn curf_formula(kind: u16, params: &[f32]) -> Vec<u8> {
        let mut curve = b"curf\0\0\0\0".to_vec();
        curve.extend_from_slice(&1u16.to_be_bytes());
        curve.extend_from_slice(&0u16.to_be_bytes());
        curve.extend_from_slice(b"parf\0\0\0\0");
        curve.extend_from_slice(&kind.to_be_bytes());
        curve.extend_from_slice(&0u16.to_be_bytes());
        for param in params {
            curve.extend_from_slice(&param.to_be_bytes());
        }
        curve
    }

    /// A formula piece computing y = x (type 6, gamma 1), appended to a curve.
    fn append_identity_piece(curve: &mut Vec<u8>) {
        curve.extend_from_slice(b"parf\0\0\0\0");
        curve.extend_from_slice(&0u16.to_be_bytes());
        curve.extend_from_slice(&0u16.to_be_bytes());
        for param in [1.0f32, 1.0, 0.0, 0.0] {
            curve.extend_from_slice(&param.to_be_bytes());
        }
    }

    /// A two-piece segmented curve: a formula below the breakpoint, a formula
    /// above it.
    fn curf_two_formulas(break_at: f32, low: (u16, Vec<f32>), high: (u16, Vec<f32>)) -> Vec<u8> {
        let mut curve = b"curf\0\0\0\0".to_vec();
        curve.extend_from_slice(&2u16.to_be_bytes());
        curve.extend_from_slice(&0u16.to_be_bytes());
        curve.extend_from_slice(&break_at.to_be_bytes());
        for (kind, params) in [low, high] {
            curve.extend_from_slice(b"parf\0\0\0\0");
            curve.extend_from_slice(&kind.to_be_bytes());
            curve.extend_from_slice(&0u16.to_be_bytes());
            for param in params {
                curve.extend_from_slice(&param.to_be_bytes());
            }
        }
        curve
    }

    fn cvst_element(curves: &[Vec<u8>; 3]) -> Vec<u8> {
        let mut element = b"cvst\0\0\0\0".to_vec();
        element.extend_from_slice(&3u16.to_be_bytes());
        element.extend_from_slice(&3u16.to_be_bytes());
        let mut body = Vec::new();
        let mut table = Vec::new();
        // Signature, reserved word, two channel counts, then three offsets.
        let mut at: usize = 12 + 24;
        for curve in curves {
            while !at.is_multiple_of(4) {
                body.push(0);
                at += 1;
            }
            table.extend_from_slice(&(at as u32).to_be_bytes());
            table.extend_from_slice(&(curve.len() as u32).to_be_bytes());
            body.extend_from_slice(curve);
            at += curve.len();
        }
        element.extend_from_slice(&table);
        element.extend_from_slice(&body);
        element
    }

    fn matf_element(matrix: &Mat3, offset: &[f64; 3]) -> Vec<u8> {
        let mut element = b"matf\0\0\0\0".to_vec();
        element.extend_from_slice(&3u16.to_be_bytes());
        element.extend_from_slice(&3u16.to_be_bytes());
        for row in matrix {
            for value in row {
                element.extend_from_slice(&(*value as f32).to_be_bytes());
            }
        }
        for value in offset {
            element.extend_from_slice(&(*value as f32).to_be_bytes());
        }
        element
    }

    fn clut_element(grid: [u8; 3], nodes: &[f64]) -> Vec<u8> {
        let mut element = b"clut\0\0\0\0".to_vec();
        element.extend_from_slice(&3u16.to_be_bytes());
        element.extend_from_slice(&3u16.to_be_bytes());
        let mut dims = [0u8; 16];
        dims[0..3].copy_from_slice(&grid);
        element.extend_from_slice(&dims);
        for value in nodes {
            element.extend_from_slice(&(*value as f32).to_be_bytes());
        }
        element
    }

    fn mpe_tag(elements: &[Vec<u8>]) -> Vec<u8> {
        let mut tag = b"mpet\0\0\0\0".to_vec();
        tag.extend_from_slice(&3u16.to_be_bytes());
        tag.extend_from_slice(&3u16.to_be_bytes());
        tag.extend_from_slice(&(elements.len() as u32).to_be_bytes());
        let mut body = Vec::new();
        let mut table = Vec::new();
        let mut at = 16 + elements.len() * 8;
        for element in elements {
            while !at.is_multiple_of(4) {
                body.push(0);
                at += 1;
            }
            table.extend_from_slice(&(at as u32).to_be_bytes());
            table.extend_from_slice(&(element.len() as u32).to_be_bytes());
            body.extend_from_slice(element);
            at += element.len();
        }
        tag.extend_from_slice(&table);
        tag.extend_from_slice(&body);
        tag
    }

    /// Linear display light from normalized PCS: undo the 1.15 scaling, then
    /// the primaries matrix. For an sRGB-primary display this is sRGB's own
    /// linear light, which is what makes the gamma profiles below comparable
    /// to the matrix path's gamma test.
    fn pcs_to_linear(xyz: [f64; 3]) -> [f64; 3] {
        let from_pcs = invert(&SRGB_D50).expect("sRGB primaries invert");
        mul_vec(
            &from_pcs,
            &[
                xyz[0] / XYZ_PCS_SCALE,
                xyz[1] / XYZ_PCS_SCALE,
                xyz[2] / XYZ_PCS_SCALE,
            ],
        )
    }

    /// A gamma-2.2 sRGB-primary display as a 16-bit LUT: identity input
    /// tables, a CLUT that turns the PCS into linear display light, and gamma
    /// output tables.
    fn lut16_gamma_profile() -> Vec<u8> {
        let input = [identity_table(2), identity_table(2), identity_table(2)];
        let clut = clut_nodes(9, pcs_to_linear);
        let output = [
            gamma_table(256, 2.2),
            gamma_table(256, 2.2),
            gamma_table(256, 2.2),
        ];
        profile_with_tags(&[(b"B2A1", lut16_tag(&IDENTITY, &input, 9, &clut, &output))])
    }

    /// The same display as a v4 `mBA ` tag: identity B curves, a matrix that
    /// undoes the PCS scaling, a small identity CLUT, gamma A curves.
    fn mba_gamma_profile() -> Vec<u8> {
        let identity_curves = [
            b"curv\0\0\0\0\0\0\0\0".to_vec(),
            b"curv\0\0\0\0\0\0\0\0".to_vec(),
            b"curv\0\0\0\0\0\0\0\0".to_vec(),
        ];
        let scale = 1.0 / XYZ_PCS_SCALE;
        let from_pcs = invert(&SRGB_D50).expect("sRGB primaries invert");
        let matrix = multiply(
            &from_pcs,
            &[[scale, 0.0, 0.0], [0.0, scale, 0.0], [0.0, 0.0, scale]],
        );
        let clut = clut_nodes(3, |xyz| xyz);
        // The A curves run forwards, so they encode: the reciprocal gamma.
        let gamma = curv_gamma(1.0 / 2.2);
        // A small black lift, the way a real panel's flare shows up in a
        // profile: it keeps every channel away from zero, where a gamma
        // curve's slope is infinite and two engines' rounding disagree.
        let tag = mba_tag(
            Some(curves_blob(&identity_curves)),
            Some(matrix_element_bytes(&matrix, &[0.02; 3])),
            None,
            Some(mba_clut_bytes([3, 3, 3], &clut)),
            Some(curves_blob(&[gamma.clone(), gamma.clone(), gamma])),
        );
        profile_with_tags(&[(b"B2A1", tag)])
    }

    /// The same display as a v4 float MPE (`B2D1`): identity B curves, the
    /// primaries matrix with the same black lift, an identity float CLUT, and
    /// gamma A curves written as segmented formulas. The float tag meets the
    /// PCS at full XYZ scale — LittleCMS normalizes it there — so unlike the
    /// 16-bit tags there is no 1.15 scaling to undo.
    fn mpe_gamma_profile() -> Vec<u8> {
        let identity = curf_formula(0, &[1.0, 1.0, 0.0, 0.0]);
        let b_curves = cvst_element(&[identity.clone(), identity.clone(), identity]);
        let matrix = invert(&SRGB_D50).expect("sRGB primaries invert");
        let clut = clut_nodes(3, |xyz| xyz);
        let gamma = curf_formula(0, &[1.0 / 2.2, 1.0, 0.0, 0.0]);
        let a_curves = cvst_element(&[gamma.clone(), gamma.clone(), gamma]);
        let tag = mpe_tag(&[
            b_curves,
            matf_element(&matrix, &[0.02; 3]),
            clut_element([3, 3, 3], &clut),
            a_curves,
        ]);
        profile_with_tags(&[(b"B2D1", tag)])
    }

    /// An MPE profile whose A curves are the sRGB encoding written as two
    /// formula segments — linear below the knee, power above — so the whole
    /// transform is the identity: sRGB in, sRGB out. The breakpoint and both
    /// formula pieces are exercised end to end.
    fn mpe_identity_profile() -> Vec<u8> {
        let identity = curf_formula(0, &[1.0, 1.0, 0.0, 0.0]);
        let b_curves = cvst_element(&[identity.clone(), identity.clone(), identity]);
        let matrix = invert(&SRGB_D50).expect("sRGB primaries invert");
        let clut = clut_nodes(3, |xyz| xyz);
        // sRGB's encode, 1.055·x^(1/2.4) − 0.055, as a type-6 piece:
        // (a·x)^g + c with a^g = 1.055, so a = 1.055^2.4.
        let srgb_encode = curf_two_formulas(
            0.003_130_8,
            (0, vec![1.0, 12.92, 0.0, 0.0]),
            (0, vec![1.0 / 2.4, 1.055f64.powf(2.4) as f32, 0.0, -0.055]),
        );
        let a_curves = cvst_element(&[srgb_encode.clone(), srgb_encode.clone(), srgb_encode]);
        let tag = mpe_tag(&[
            b_curves,
            matf_element(&matrix, &[0.0; 3]),
            clut_element([3, 3, 3], &clut),
            a_curves,
        ]);
        profile_with_tags(&[(b"B2D1", tag)])
    }

    /// Write the cross-check profiles where LittleCMS can read them, and print
    /// this engine's answer beside the colours. Run with `--ignored`, then
    /// `transicc -o <file> -t1 -n` over the same colours; the numbers it prints
    /// are what the assertions below pin. Not a check itself — the generator
    /// of the checks' ground truth.
    #[test]
    #[ignore]
    fn dump_lut_cross_check_fixtures() {
        let dir = std::path::Path::new("/tmp/icc-lut-fixtures");
        std::fs::create_dir_all(dir).unwrap();
        let colours = [
            [1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [0.0, 0.0, 1.0],
            [0.501_960_8, 0.501_960_8, 0.501_960_8],
            [0.2, 0.4, 0.6],
        ];
        for (name, bytes) in [
            ("lut16_gamma.icc", lut16_gamma_profile()),
            ("mba_gamma.icc", mba_gamma_profile()),
            ("mpe_gamma.icc", mpe_gamma_profile()),
            ("mpe_identity.icc", mpe_identity_profile()),
        ] {
            std::fs::write(dir.join(name), &bytes).unwrap();
            let transform = DisplayTransform::for_profile(&bytes).expect("a transform");
            for colour in colours {
                let mapped = transform.map(colour);
                println!("{name} {colour:?} -> {mapped:?}");
            }
        }
    }

    /// A 16-bit LUT display: these numbers are LittleCMS 2.19's own answer for
    /// the same profile (sRGB in, relative colorimetric), converted from its
    /// 8-bit output — the check that the LUT path is a colour transform and
    /// not a plausible-looking one. Regenerate with the dump test above.
    #[test]
    fn a_lut16_display_matches_lcms2() {
        let transform = DisplayTransform::for_profile(&lut16_gamma_profile()).expect("a transform");
        let expected = [
            ([1.0, 0.0, 0.0], [0.9463, 0.2154, 0.1702]),
            ([0.0, 1.0, 0.0], [0.4142, 0.9709, 0.3238]),
            ([0.0, 0.0, 1.0], [0.4088, 0.3032, 0.9803]),
            (
                [0.501_960_8, 0.501_960_8, 0.501_960_8],
                [0.5111, 0.4981, 0.5048],
            ),
            ([0.2, 0.4, 0.6], [0.3728, 0.3995, 0.5944]),
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

    /// The same cross-check for a v4 `mBA ` tag: B curves, a matrix with a
    /// black lift, a CLUT and gamma A curves.
    #[test]
    fn an_mba_display_matches_lcms2() {
        let transform = DisplayTransform::for_profile(&mba_gamma_profile()).expect("a transform");
        let expected = [
            ([1.0, 0.0, 0.0], [1.0, 0.1699, 0.1698]),
            ([0.0, 1.0, 0.0], [0.1704, 1.0, 0.1697]),
            ([0.0, 0.0, 1.0], [0.1700, 0.1700, 1.0]),
            (
                [0.501_960_8, 0.501_960_8, 0.501_960_8],
                [0.5197, 0.5197, 0.5196],
            ),
            ([0.2, 0.4, 0.6], [0.2645, 0.4270, 0.6121]),
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

    /// The same cross-check for a v4 float MPE (`B2D1`): segmented-curve sets,
    /// a matrix with offset, and a float CLUT. The float tag meets the PCS at
    /// full XYZ scale, which is the difference this pins against the 16-bit
    /// tags above.
    #[test]
    fn an_mpe_display_matches_lcms2() {
        let transform = DisplayTransform::for_profile(&mpe_gamma_profile()).expect("a transform");
        let expected = [
            ([1.0, 0.0, 0.0], [1.0, 0.1689, 0.1689]),
            ([0.0, 1.0, 0.0], [0.1694, 1.0, 0.1687]),
            ([0.0, 0.0, 1.0], [0.1691, 0.1690, 1.0]),
            (
                [0.501_960_8, 0.501_960_8, 0.501_960_8],
                [0.5186, 0.5186, 0.5185],
            ),
            ([0.2, 0.4, 0.6], [0.2634, 0.4258, 0.6111]),
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

    /// An MPE profile that describes sRGB itself leaves every colour alone —
    /// the identity, end to end through segmented formula curves, checked
    /// against both LittleCMS and the input it should return.
    #[test]
    fn an_mpe_identity_profile_leaves_colours_alone() {
        let transform =
            DisplayTransform::for_profile(&mpe_identity_profile()).expect("a transform");
        let expected = [
            ([1.0, 0.0, 0.0], [1.0, 0.0, 0.0]),
            ([0.0, 1.0, 0.0], [0.0016, 1.0, 0.0]),
            ([0.0, 0.0, 1.0], [0.0004, 0.0002, 0.9998]),
            (
                [0.501_960_8, 0.501_960_8, 0.501_960_8],
                [0.5020, 0.5020, 0.5019],
            ),
            ([0.2, 0.4, 0.6], [0.2001, 0.4000, 0.5999]),
        ];
        for (input, wanted) in expected {
            let mapped = transform.map(input);
            for channel in 0..3 {
                assert!(
                    (mapped[channel] - wanted[channel]).abs() < 3e-3,
                    "{input:?} became {mapped:?}, LittleCMS says {wanted:?}"
                );
                assert!(
                    (mapped[channel] - input[channel]).abs() < 3e-3,
                    "{input:?} became {mapped:?}, the identity should return it"
                );
            }
        }
    }

    /// The tetrahedral CLUT reproduces an affine function exactly — the
    /// property the cross-check profiles lean on — and clamps its input.
    #[test]
    fn a_clut_interpolates_affine_exactly() {
        let grid = 5;
        let nodes = clut_nodes(grid, |xyz| {
            [
                0.2 + 0.5 * xyz[0] - 0.1 * xyz[1],
                0.1 + 0.3 * xyz[1] + 0.2 * xyz[2],
                0.4 - 0.2 * xyz[0] + 0.3 * xyz[2],
            ]
        });
        let clut = Clut::new([grid; 3], nodes).expect("a clut");
        for input in [
            [0.0, 0.0, 0.0],
            [1.0, 1.0, 1.0],
            [0.13, 0.71, 0.42],
            [0.5, 0.5, 0.5],
            [0.25, 0.25, 0.75],
        ] {
            let want = [
                0.2 + 0.5 * input[0] - 0.1 * input[1],
                0.1 + 0.3 * input[1] + 0.2 * input[2],
                0.4 - 0.2 * input[0] + 0.3 * input[2],
            ];
            let got = clut.eval(input);
            for channel in 0..3 {
                assert!(
                    (got[channel] - want[channel]).abs() < 1e-9,
                    "{input:?} became {got:?}, want {want:?}"
                );
            }
        }
        // Out-of-range input is clamped to the grid, not read past it.
        let edge = clut.eval([2.0, -1.0, 0.5]);
        let want = clut.eval([1.0, 0.0, 0.5]);
        assert_eq!(edge, want);
    }

    /// A sampled segmented piece takes its first value from the piece below
    /// the breakpoint, and the breakpoint belongs to the lower piece. The
    /// sampled piece has finite bounds on both sides, the way a real profile
    /// writes it — the open ends of the domain are formula pieces.
    #[test]
    fn a_segmented_curve_is_continuous_at_its_breakpoint() {
        // y = x below 0.5 and above 0.9 (formula type 6, gamma 1); sampled
        // 0.5 → 0.7 → 0.9 between, first point implicit.
        let mut curve = b"curf\0\0\0\0".to_vec();
        curve.extend_from_slice(&3u16.to_be_bytes());
        curve.extend_from_slice(&0u16.to_be_bytes());
        for break_at in [0.5f32, 0.9] {
            curve.extend_from_slice(&break_at.to_be_bytes());
        }
        append_identity_piece(&mut curve);
        curve.extend_from_slice(b"samf\0\0\0\0");
        curve.extend_from_slice(&2u32.to_be_bytes());
        for value in [0.7f32, 0.9] {
            curve.extend_from_slice(&value.to_be_bytes());
        }
        append_identity_piece(&mut curve);
        let table = segmented_table(&curve).expect("a curve");
        // The implicit first point is the lower piece's value at 0.5.
        assert!((table.eval(0.5) - 0.5).abs() < 1e-6);
        assert!((table.eval(0.25) - 0.25).abs() < 1e-6);
        // Inside the sampled piece the samples run 0.5 → 0.7 → 0.9.
        assert!((table.eval(0.75) - 0.75).abs() < 1e-6);
        assert!((table.eval(0.625) - 0.625).abs() < 1e-6);
        // The upper formula piece takes over past 0.9.
        assert!((table.eval(1.0) - 1.0).abs() < 1e-6);
    }

    /// The three segmented formula types evaluate as LittleCMS evaluates them.
    /// The tolerance is float32's: the parameters cross the wire as f32.
    #[test]
    fn segmented_formula_types_match_their_definitions() {
        // Type 6: (a·x + b)^g + c.
        let t6 = segmented_table(&curf_formula(0, &[2.0, 1.0, 0.0, 0.1])).unwrap();
        assert!((t6.eval(0.5) - (0.5f64.powf(2.0) + 0.1)).abs() < 1e-6);
        // Type 7: a·log10(b·x^g + c) + d.
        let t7 = segmented_table(&curf_formula(1, &[1.0, 2.0, 1.0, 0.0, 0.0])).unwrap();
        assert!((t7.eval(0.5) - 2.0 * 0.5f64.log10()).abs() < 1e-6);
        // Type 8: a·b^(c·x + d) + e.
        let t8 = segmented_table(&curf_formula(2, &[1.0, 2.0, 1.0, 0.0, 0.0])).unwrap();
        assert!((t8.eval(0.5) - 2.0f64.powf(0.5)).abs() < 1e-6);
    }

    /// A LUT profile bakes the same cube shape the matrix path does.
    #[test]
    fn a_lut_profile_bakes_the_documented_cube() {
        let transform = DisplayTransform::for_profile(&lut16_gamma_profile()).expect("a transform");
        let lut = transform.lut();
        assert_eq!(lut.len(), LUT_SIZE * LUT_SIZE * LUT_SIZE);
        assert!(lut.iter().all(|entry| entry[3] == 1.0));
        assert!(lut[0][0] < 1e-3 && lut[0][1] < 1e-3 && lut[0][2] < 1e-3);
        // White lands near full scale; a 16-bit CLUT is a sampled transform,
        // so its corners are approximate where the grid nodes clamp.
        let last = lut[lut.len() - 1];
        assert!(last.iter().take(3).all(|&v| v > 0.9), "{last:?}");
    }

    /// The matrix description wins when a profile carries both, so a profile
    /// that worked before keeps the transform it shipped with.
    #[test]
    fn a_profile_with_both_descriptions_keeps_the_matrix() {
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
            (b"rTRC", srgb_curve()),
            (b"gTRC", srgb_curve()),
            (b"bTRC", srgb_curve()),
            (
                b"B2A1",
                lut16_tag(
                    &IDENTITY,
                    &[identity_table(2), identity_table(2), identity_table(2)],
                    2,
                    &clut_nodes(2, |_| [1.0, 0.0, 0.0]),
                    &[identity_table(2), identity_table(2), identity_table(2)],
                ),
            ),
        ];
        tags.sort_by(|a, b| a.0.cmp(b.0));
        let transform = DisplayTransform::for_profile(&profile_with_tags(&tags)).unwrap();
        // The LUT would paint everything red; the matrix leaves grey alone.
        let grey = transform.map([0.5, 0.5, 0.5]);
        assert!((grey[0] - 0.5).abs() < 3e-3, "{grey:?}");
    }

    /// Profiles this cannot honestly transform stay refused: the screen keeps
    /// whatever it had rather than guessing.
    #[test]
    fn lut_profiles_that_cannot_be_read_are_refused() {
        // A truncated tag, a grid of one, an unknown type, a non-RGB channel
        // count, and a CLUT precision that is neither byte width.
        let good = lut16_gamma_profile();
        let mut truncated = good.clone();
        truncated.truncate(truncated.len() - 10);
        assert!(DisplayTransform::for_profile(&truncated).is_none());

        let mut grid_one = lut16_tag(
            &IDENTITY,
            &[identity_table(2), identity_table(2), identity_table(2)],
            1,
            &[],
            &[identity_table(2), identity_table(2), identity_table(2)],
        );
        assert!(
            DisplayTransform::for_profile(&profile_with_tags(&[(b"B2A1", grid_one.clone())]))
                .is_none()
        );
        grid_one[10] = 2; // a legal grid, but the CLUT nodes are missing
        assert!(
            DisplayTransform::for_profile(&profile_with_tags(&[(b"B2A1", grid_one)])).is_none()
        );

        let mut unknown_tag = lut16_tag(
            &IDENTITY,
            &[identity_table(2), identity_table(2), identity_table(2)],
            2,
            &clut_nodes(2, |xyz| xyz),
            &[identity_table(2), identity_table(2), identity_table(2)],
        );
        unknown_tag[0..4].copy_from_slice(b"mft9");
        assert!(
            DisplayTransform::for_profile(&profile_with_tags(&[(b"B2A1", unknown_tag)])).is_none()
        );

        let mut cmyk = lut16_tag(
            &IDENTITY,
            &[identity_table(2), identity_table(2), identity_table(2)],
            2,
            &clut_nodes(2, |xyz| xyz),
            &[identity_table(2), identity_table(2), identity_table(2)],
        );
        cmyk[8] = 4;
        assert!(DisplayTransform::for_profile(&profile_with_tags(&[(b"B2A1", cmyk)])).is_none());

        // Only a device → PCS table: the direction this would have to invert.
        let a2b_only = mba_tag(
            Some(curves_blob(&[
                b"curv\0\0\0\0\0\0\0\0".to_vec(),
                b"curv\0\0\0\0\0\0\0\0".to_vec(),
                b"curv\0\0\0\0\0\0\0\0".to_vec(),
            ])),
            None,
            None,
            None,
            None,
        );
        assert!(
            DisplayTransform::for_profile(&profile_with_tags(&[(b"A2B0", a2b_only)])).is_none()
        );

        // Only the saturation intent: relative colorimetric does not fall
        // back to it, the same answer LittleCMS gives.
        let saturation = lut16_tag(
            &IDENTITY,
            &[identity_table(2), identity_table(2), identity_table(2)],
            2,
            &clut_nodes(2, |xyz| xyz),
            &[identity_table(2), identity_table(2), identity_table(2)],
        );
        assert!(
            DisplayTransform::for_profile(&profile_with_tags(&[(b"B2A2", saturation)])).is_none()
        );

        // A grid past the entry cap is refused before its nodes are read:
        // 89³ × 3 is over the cap, and the header alone says so.
        let mut huge = mba_clut_bytes([89, 89, 89], &[]);
        huge.truncate(20);
        let huge_tag = mba_tag(None, None, None, Some(huge), None);
        assert!(
            DisplayTransform::for_profile(&profile_with_tags(&[(b"B2A1", huge_tag)])).is_none()
        );
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
