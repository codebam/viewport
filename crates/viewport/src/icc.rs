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

/// Legacy v2 16-bit Lab (`mft2`) places `L* = 100` at `0xFF00` and neutral
/// `a* = b* = 0` at `0x8000`, so a v4-normalized `[0, 1]` Lab triple meets a
/// v2 16-bit table scaled by `0xFF00 / 0xFFFF` (`256 / 257`).
const LAB_V2_SCALE: f64 = 65280.0 / 65535.0;

/// The ICC PCS illuminant (CIE D50), matching LittleCMS's `cmsD50X/Y/Z`.
const D50_XYZ: [f64; 3] = [0.9642, 1.0, 0.8249];

/// CIE 15:2004 `f(t)` for XYZ → L*a*b*, with the `(24/116)³` continuity knee.
fn lab_f(t: f64) -> f64 {
    const DELTA: f64 = 24.0 / 116.0;
    const LIMIT: f64 = DELTA * DELTA * DELTA;
    if t <= LIMIT {
        (841.0 / 108.0) * t + (16.0 / 116.0)
    } else {
        t.cbrt()
    }
}

/// Convert CIE XYZ (relative to D50) to CIE 1976 `[L*, a*, b*]`.
fn xyz_to_lab(xyz: [f64; 3]) -> [f64; 3] {
    let fx = lab_f(xyz[0] / D50_XYZ[0]);
    let fy = lab_f(xyz[1] / D50_XYZ[1]);
    let fz = lab_f(xyz[2] / D50_XYZ[2]);
    [116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz)]
}

/// The Profile Connection Space declared in bytes 20..24 of the ICC header.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pcs {
    Xyz,
    Lab,
}

/// How the PCS triple is encoded at the boundary of a specific LUT tag type.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PcsEncoding {
    /// 8-bit or 16-bit XYZ in 1.15 fixed point (`value × 32768 / 65535`).
    Xyz16,
    /// v4 float XYZ (`mpet`), unscaled (`Y = 1.0` at media white).
    XyzFloat,
    /// Legacy v2 16-bit Lab (`mft2`): `L* 0..100` on `0..0xFF00` and `a*/b*`
    /// `-128..+127.996` on `0..0xFFFF` (`0` at `0x8000`).
    LabV2,
    /// v4 16-bit Lab (`mBA `/`mAB `) and v2 8-bit Lab (`mft1`): `L* 0..100` on
    /// `0..0xFFFF` (`0..0xFF`) and `a*/b*` `-128..+127` (`0` at `0x8080`/`0x80`).
    LabV4,
    /// v4 float Lab (`mpet`): direct CIE `L*` (`0..100`) and `a*/b*`
    /// (`-128..+127`) numbers, as LittleCMS's `_cmsStageNormalizeToLabFloat`
    /// and `_cmsStageNormalizeFromLabFloat` hand to the tag.
    LabFloat,
}

impl PcsEncoding {
    fn encode(self, xyz: [f64; 3]) -> [f64; 3] {
        match self {
            Self::Xyz16 => [
                xyz[0] * XYZ_PCS_SCALE,
                xyz[1] * XYZ_PCS_SCALE,
                xyz[2] * XYZ_PCS_SCALE,
            ],
            Self::XyzFloat => xyz,
            Self::LabV2 => {
                let [l, a, b] = xyz_to_lab(xyz);
                [
                    (l / 100.0) * LAB_V2_SCALE,
                    ((a + 128.0) / 255.0) * LAB_V2_SCALE,
                    ((b + 128.0) / 255.0) * LAB_V2_SCALE,
                ]
            }
            Self::LabV4 => {
                let [l, a, b] = xyz_to_lab(xyz);
                [l / 100.0, (a + 128.0) / 255.0, (b + 128.0) / 255.0]
            }
            Self::LabFloat => xyz_to_lab(xyz),
        }
    }
}

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
/// encoded. A LUT-described profile says the same thing as a table: when it
/// carries a `B2A`/`B2D` (PCS → device) tag, that table is read forwards; when
/// it carries only an `A2B`/`D2B` (device → PCS) tag, the pipeline is inverted
/// at bake time — output curves inverted, the 3D CLUT solved by Newton on the
/// tetrahedral interpolant with bounded bisection as the fallback, input curves
/// and matrices inverted, and out-of-gamut PCS clamped deterministically to the
/// device gamut boundary. Both `XYZ ` and `Lab ` PCS are read. Either way,
/// `lut` is that function sampled on a cube, which is the form the renderer can
/// upload as a 3D texture.
///
/// The matrix description is tried first, so a profile that carries both keeps
/// the transform it shipped with; the LUT is what a profile with no primaries
/// and curves is read through. What is still refused: a non-RGB device space,
/// any present tag this cannot read whole, and any curve, matrix or CLUT whose
/// inverse is not well-defined — a profile that cannot be understood is not a
/// reason to guess a transform, and the log names the rule that refused it.
pub struct DisplayTransform {
    kind: Kind,
    /// sRGB encoded → PCS XYZ D50.
    srgb_to_pcs: Mat3,
    /// `r + g*N + b*N²` entries, each `[r, g, b, 1.0]`, display encoded.
    lut: Vec<[f32; 4]>,
}

/// The ways a display profile can say what the panel does.
enum Kind {
    /// Three primaries and three transfer curves, read backwards.
    Matrix {
        /// display encoded → linear light, per channel.
        decode: [Curve; 3],
        /// PCS XYZ D50 → linear display RGB.
        from_pcs: Mat3,
    },
    /// A PCS → device table (`B2D`/`B2A`), read forwards.
    Lut(LutPipeline),
    /// A device → PCS table (`D2B`/`A2B`), inverted stage-by-stage.
    InvertedLut(InvertedLutPipeline),
}

impl DisplayTransform {
    /// Read a display profile: matrix-shaper first, then a PCS → device LUT,
    /// then an inverted device → PCS LUT. Anything else logs the rule that
    /// refused it and returns `None`: a profile that cannot be understood is
    /// not a reason to guess a transform.
    pub fn for_profile(profile: &[u8]) -> Option<Self> {
        match Self::parse(profile) {
            Ok(transform) => Some(transform),
            Err(reason) => {
                tracing::debug!("ICC display transform refused: {reason}");
                None
            }
        }
    }

    fn parse(profile: &[u8]) -> Result<Self, &'static str> {
        if profile.len() < 132 || &profile[36..40] != b"acsp" {
            return Err("header too short or missing 'acsp' signature");
        }
        let count = be_u32(&profile[128..132]) as usize;
        if count > (profile.len() - 132) / 12 {
            return Err("tag directory count exceeds profile length");
        }
        // The transform is RGB in, RGB out; an XYZ, Lab, CMYK or Gray device
        // space is a different question with a different answer.
        if &profile[16..20] != b"RGB " {
            return Err("non-RGB device colour space");
        }
        let pcs = match &profile[20..24] {
            b"XYZ " => Pcs::Xyz,
            b"Lab " => Pcs::Lab,
            _ => return Err("unsupported PCS in header (expected 'XYZ ' or 'Lab ')"),
        };
        let kind = match Self::matrix_kind(profile)? {
            Some(kind) => kind,
            None => Self::lut_kind(profile, pcs)?,
        };
        let mut transform = Self {
            kind,
            srgb_to_pcs: multiply(&BRADFORD_D65_TO_D50, &SRGB_TO_XYZ_D65),
            lut: Vec::new(),
        };
        transform.lut = transform.bake();
        Ok(transform)
    }

    fn matrix_kind(profile: &[u8]) -> Result<Option<Kind>, &'static str> {
        const TAGS: [&[u8; 4]; 6] = [b"rXYZ", b"gXYZ", b"bXYZ", b"rTRC", b"gTRC", b"bTRC"];
        let mut slices = [&[][..]; 6];
        let mut present = 0usize;
        for (i, sig) in TAGS.iter().enumerate() {
            match lookup_tag(profile, sig) {
                TagLookup::Absent => {}
                TagLookup::InvalidSlice => {
                    return Err("matrix-shaper tag slice exceeds profile bounds");
                }
                TagLookup::Present(data) => {
                    slices[i] = data;
                    present += 1;
                }
            }
        }
        if present < 6 {
            // Validate any partial colorant or curve tags that are present so a
            // corrupt tag is still refused, then let a LUT profile without TRC
            // curves proceed to lut_kind.
            for &slice in &slices[0..3] {
                if !slice.is_empty() && xyz_tag(slice).is_none() {
                    return Err("rXYZ/gXYZ/bXYZ tag is present but cannot be read whole");
                }
            }
            for &slice in &slices[3..6] {
                if !slice.is_empty() && Curve::from_tag(slice).is_none() {
                    return Err("rTRC/gTRC/bTRC tag is present but cannot be read or inverted");
                }
            }
            return Ok(None);
        }
        let r = xyz_tag(slices[0]).ok_or("rXYZ tag cannot be read whole")?;
        let g = xyz_tag(slices[1]).ok_or("gXYZ tag cannot be read whole")?;
        let b = xyz_tag(slices[2]).ok_or("bXYZ tag cannot be read whole")?;
        let to_pcs = [[r[0], g[0], b[0]], [r[1], g[1], b[1]], [r[2], g[2], b[2]]];
        let from_pcs = invert(&to_pcs).ok_or("matrix-shaper primaries matrix is singular")?;
        let decode = [
            Curve::from_tag(slices[3]).ok_or("rTRC curve cannot be read or is non-monotone")?,
            Curve::from_tag(slices[4]).ok_or("gTRC curve cannot be read or is non-monotone")?,
            Curve::from_tag(slices[5]).ok_or("bTRC curve cannot be read or is non-monotone")?,
        ];
        Ok(Some(Kind::Matrix { decode, from_pcs }))
    }

    fn lut_kind(profile: &[u8], pcs: Pcs) -> Result<Kind, &'static str> {
        // The order LittleCMS reads an output profile in for relative
        // colorimetric: the v4 float tag first, then the 16-bit
        // relative-colorimetric tag, then — only when that tag is absent —
        // the perceptual one. A tag that is present but cannot be read is a
        // refusal, not an invitation to try the next: half a transform is
        // worse than none.
        for sig in [b"B2D1", b"B2A1", b"B2A0"] {
            match lookup_tag(profile, sig) {
                TagLookup::Absent => continue,
                TagLookup::InvalidSlice => {
                    return Err("B2A/B2D tag offset or size exceeds profile bounds");
                }
                TagLookup::Present(data) => {
                    let pipeline = parse_b2a_tag(data, pcs)
                        .ok_or("B2A/B2D tag is present but cannot be read whole")?;
                    return Ok(Kind::Lut(pipeline));
                }
            }
        }
        // Only when no PCS → device tag is present, look for a device → PCS
        // tag to invert: v4 float D2B1 first, then A2B1 (relative
        // colorimetric), then A2B0 (perceptual), then A2B2 (saturation).
        for sig in [b"D2B1", b"A2B1", b"A2B0", b"A2B2"] {
            match lookup_tag(profile, sig) {
                TagLookup::Absent => continue,
                TagLookup::InvalidSlice => {
                    return Err("A2B/D2B tag offset or size exceeds profile bounds");
                }
                TagLookup::Present(data) => {
                    let forward = parse_a2b_tag(data, pcs)
                        .ok_or("A2B/D2B tag is present but cannot be read whole")?;
                    let inverted = InvertedLutPipeline::from_forward(forward)?;
                    return Ok(Kind::InvertedLut(inverted));
                }
            }
        }
        Err("profile carries no complete matrix-shaper, B2A/B2D, or A2B/D2B transform")
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
                    // `finite_clamp`, not `clamp`: f64::clamp passes NaN
                    // through — a NaN channel in is a NaN channel out,
                    // which samples as a black pixel. The LUT paths below
                    // already clamp this way at their stage boundaries.
                    *value = finite_clamp(decode[channel].invert(finite_clamp(device[channel])));
                }
                out
            }
            Kind::Lut(pipeline) => {
                // The table itself does the gamut mapping its intent
                // describes; the screen clips what is left.
                let encoded_pcs = pipeline.pcs_encoding.encode(pcs);
                let device = pipeline.eval(encoded_pcs);
                [
                    finite_clamp(device[0]),
                    finite_clamp(device[1]),
                    finite_clamp(device[2]),
                ]
            }
            Kind::InvertedLut(pipeline) => {
                let encoded_pcs = pipeline.pcs_encoding.encode(pcs);
                let device = pipeline.eval(encoded_pcs);
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

/// Whether a tag is absent from the directory, points outside the file, or has
/// a slice ready to parse.
enum TagLookup<'a> {
    Absent,
    InvalidSlice,
    Present(&'a [u8]),
}

/// Find one tag's data in a profile, distinguishing an absent tag from a
/// directory entry whose offset or size points past the end of the file.
fn lookup_tag<'a>(profile: &'a [u8], signature: &[u8; 4]) -> TagLookup<'a> {
    let count = be_u32(&profile[128..132]) as usize;
    if count > (profile.len() - 132) / 12 {
        return TagLookup::InvalidSlice;
    }
    let wanted = u32::from_be_bytes(*signature);
    for at in (132..132 + count * 12).step_by(12) {
        if be_u32(&profile[at..at + 4]) != wanted {
            continue;
        }
        let offset = be_u32(&profile[at + 4..at + 8]) as usize;
        let size = be_u32(&profile[at + 8..at + 12]) as usize;
        let Some(end) = offset.checked_add(size) else {
            return TagLookup::InvalidSlice;
        };
        if size == 0 || end > profile.len() {
            return TagLookup::InvalidSlice;
        }
        return TagLookup::Present(&profile[offset..end]);
    }
    TagLookup::Absent
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

fn determinant(m: &Mat3) -> f64 {
    m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
        - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0])
}

fn invert(matrix: &Mat3) -> Option<Mat3> {
    let m = matrix;
    let det = determinant(m);
    // A singular matrix is a profile with no answer, not one with a zero
    // transform: refusing keeps the screen as it was instead of blacking it.
    if !det.is_finite() || det.abs() < 1e-9 {
        return None;
    }
    let scale = det.recip();
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
// that mixes the channels, and sometimes a matrix. When the profile carries a
// `B2A` (v2 and v4) or `B2D` (v4 float) tag, it already runs PCS to device and
// is read forwards; when it carries only an `A2B` or `D2B` (device to PCS)
// tag, the stages are inverted in reverse order at bake time. The five tag
// types this reads (`mft1`, `mft2`, `mBA `, `mAB `, `mpet`) all decompose into
// the same three stage kinds.
//
// The reference for every layout and every evaluation rule below is LittleCMS
// 2.19, the same engine the matrix path is cross-checked against: the stage
// order, tetrahedral CLUT interpolation (and trilinear when a 16-bit/8-bit
// `B2A` CLUT is indexed in Lab), the 1.15 fixed-point XYZ and v2/v4 Lab PCS
// encodings, and the segmented-curve formulas are its behaviour, read out of
// its source rather than guessed from the specification's prose.

/// The most entries one CLUT may hold. 255 points per dimension is legal ICC
/// and would be half a gigabyte of f64; real display LUTs are 17³ to 65³, and
/// this cap (about 87³ uniform) leaves every real profile room while a corrupt
/// grid cannot allocate without bound.
const CLUT_ENTRY_CAP: usize = 1 << 21;

/// The most segments one segmented curve may carry, and the most samples across
/// them — a real curve has one to three segments; these bound a corrupt count.
const SEGMENT_CAP: usize = 1024;
const SEGMENT_SAMPLE_CAP: usize = 1 << 16;

/// The most CLUT-bearing stages one MPE pipeline may carry. A real display
/// profile has one; the position-table format would allow a chain of 256,
/// and in an inverted (`D2B`) chain every stage multiplies the bake: a CLUT
/// whose output range the incoming PCS never reaches puts every vertex on
/// the bounded bisection fallback, and a few hundred such stages is minutes
/// of solving out of a file kilobytes long. Four is two to four times the
/// wildest pipeline a profiler writes.
const PIPELINE_CLUT_CAP: usize = 4;

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
    /// 3D CLUT. The six tetrahedra partition the cell by the ordering of the
    /// three fractional coordinates; within each, the answer is affine, so the
    /// four node differences below reproduce it exactly.
    fn eval(&self, input: [f64; 3]) -> [f64; 3] {
        self.eval_with_jacobian(input).0
    }

    /// Evaluate the tetrahedral interpolant and its exact local Jacobian
    /// `∂output / ∂input` inside the active tetrahedron.
    fn eval_with_jacobian(&self, input: [f64; 3]) -> ([f64; 3], Mat3) {
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
            (scaled[0].floor() as usize).min(self.grid[0] - 2),
            (scaled[1].floor() as usize).min(self.grid[1] - 2),
            (scaled[2].floor() as usize).min(self.grid[2] - 2),
        ];
        let (rx, ry, rz) = (
            scaled[0] - base[0] as f64,
            scaled[1] - base[1] as f64,
            scaled[2] - base[2] as f64,
        );
        let stride = [self.grid[1] * self.grid[2] * 3, self.grid[2] * 3, 3];
        let lo = [
            base[0] * stride[0],
            base[1] * stride[1],
            base[2] * stride[2],
        ];
        let hi = [lo[0] + stride[0], lo[1] + stride[1], lo[2] + stride[2]];
        let node = |x: usize, y: usize, z: usize, channel: usize| self.table[x + y + z + channel];
        let mut out = [0.0; 3];
        let mut jacobian = [[0.0; 3]; 3];
        for channel in 0..3 {
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
            } else {
                (
                    node(hi[0], hi[1], hi[2], channel) - node(lo[0], hi[1], hi[2], channel),
                    node(lo[0], hi[1], hi[2], channel) - node(lo[0], lo[1], hi[2], channel),
                    node(lo[0], lo[1], hi[2], channel) - c0,
                )
            };
            out[channel] = c0 + c1 * rx + c2 * ry + c3 * rz;
            jacobian[channel] = [c1 * domain[0], c2 * domain[1], c3 * domain[2]];
        }
        (out, jacobian)
    }

    /// Trilinear interpolation: LittleCMS switches to this (`cmsio1.c:623`)
    /// when a 16-bit or 8-bit `B2A` CLUT is indexed in Lab space.
    fn eval_trilinear(&self, input: [f64; 3]) -> [f64; 3] {
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
            (scaled[0].floor() as usize).min(self.grid[0] - 2),
            (scaled[1].floor() as usize).min(self.grid[1] - 2),
            (scaled[2].floor() as usize).min(self.grid[2] - 2),
        ];
        let (rx, ry, rz) = (
            scaled[0] - base[0] as f64,
            scaled[1] - base[1] as f64,
            scaled[2] - base[2] as f64,
        );
        let stride = [self.grid[1] * self.grid[2] * 3, self.grid[2] * 3, 3];
        let lo = [
            base[0] * stride[0],
            base[1] * stride[1],
            base[2] * stride[2],
        ];
        let hi = [lo[0] + stride[0], lo[1] + stride[1], lo[2] + stride[2]];
        let node = |x: usize, y: usize, z: usize, channel: usize| self.table[x + y + z + channel];
        let lerp = |t: f64, a: f64, b: f64| a + (b - a) * t;
        let mut out = [0.0; 3];
        for (channel, out_value) in out.iter_mut().enumerate() {
            let d000 = node(lo[0], lo[1], lo[2], channel);
            let d001 = node(lo[0], lo[1], hi[2], channel);
            let d010 = node(lo[0], hi[1], lo[2], channel);
            let d011 = node(lo[0], hi[1], hi[2], channel);
            let d100 = node(hi[0], lo[1], lo[2], channel);
            let d101 = node(hi[0], lo[1], hi[2], channel);
            let d110 = node(hi[0], hi[1], lo[2], channel);
            let d111 = node(hi[0], hi[1], hi[2], channel);
            let dx00 = lerp(rx, d000, d100);
            let dx01 = lerp(rx, d001, d101);
            let dx10 = lerp(rx, d010, d110);
            let dx11 = lerp(rx, d011, d111);
            let dxy0 = lerp(ry, dx00, dx10);
            let dxy1 = lerp(ry, dx01, dx11);
            *out_value = lerp(rz, dxy0, dxy1);
        }
        out
    }

    /// Test all six affine tetrahedra inside `cell` for an exact preimage of
    /// `target`. Because the Sakamoto interpolant is linear on each
    /// tetrahedron, any in-cell root is given in closed form by a 3×3 solve.
    fn try_solve_cell(&self, cell: [usize; 3], target: [f64; 3]) -> Option<[f64; 3]> {
        let stride = [self.grid[1] * self.grid[2] * 3, self.grid[2] * 3, 3];
        let lo = [
            cell[0] * stride[0],
            cell[1] * stride[1],
            cell[2] * stride[2],
        ];
        let hi = [lo[0] + stride[0], lo[1] + stride[1], lo[2] + stride[2]];
        let node = |x: usize, y: usize, z: usize| -> [f64; 3] {
            let at = x + y + z;
            [self.table[at], self.table[at + 1], self.table[at + 2]]
        };
        let n000 = node(lo[0], lo[1], lo[2]);
        let n001 = node(lo[0], lo[1], hi[2]);
        let n010 = node(lo[0], hi[1], lo[2]);
        let n011 = node(lo[0], hi[1], hi[2]);
        let n100 = node(hi[0], lo[1], lo[2]);
        let n101 = node(hi[0], lo[1], hi[2]);
        let n110 = node(hi[0], hi[1], lo[2]);
        let n111 = node(hi[0], hi[1], hi[2]);
        let sub = |a: [f64; 3], b: [f64; 3]| [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
        let rhs = sub(target, n000);
        struct Tetrahedron {
            c1: [f64; 3],
            c2: [f64; 3],
            c3: [f64; 3],
            order_ok: fn(f64, f64, f64) -> bool,
        }
        let tetrahedra = [
            Tetrahedron {
                c1: sub(n100, n000),
                c2: sub(n110, n100),
                c3: sub(n111, n110),
                order_ok: |rx, ry, rz| rx >= ry - 1e-7 && ry >= rz - 1e-7,
            },
            Tetrahedron {
                c1: sub(n100, n000),
                c2: sub(n111, n101),
                c3: sub(n101, n100),
                order_ok: |rx, ry, rz| rx >= rz - 1e-7 && rz >= ry - 1e-7,
            },
            Tetrahedron {
                c1: sub(n101, n001),
                c2: sub(n111, n101),
                c3: sub(n001, n000),
                order_ok: |rx, ry, rz| rz >= rx - 1e-7 && rx >= ry - 1e-7,
            },
            Tetrahedron {
                c1: sub(n110, n010),
                c2: sub(n010, n000),
                c3: sub(n111, n110),
                order_ok: |rx, ry, rz| ry >= rx - 1e-7 && rx >= rz - 1e-7,
            },
            Tetrahedron {
                c1: sub(n111, n011),
                c2: sub(n010, n000),
                c3: sub(n011, n010),
                order_ok: |rx, ry, rz| ry >= rz - 1e-7 && rz >= rx - 1e-7,
            },
            Tetrahedron {
                c1: sub(n111, n011),
                c2: sub(n011, n001),
                c3: sub(n001, n000),
                order_ok: |rx, ry, rz| rz >= ry - 1e-7 && ry >= rx - 1e-7,
            },
        ];
        let domain = [
            (self.grid[0] - 1) as f64,
            (self.grid[1] - 1) as f64,
            (self.grid[2] - 1) as f64,
        ];
        for tet in tetrahedra {
            let (c1, c2, c3, order_ok) = (tet.c1, tet.c2, tet.c3, tet.order_ok);
            let mat = [
                [c1[0], c2[0], c3[0]],
                [c1[1], c2[1], c3[1]],
                [c1[2], c2[2], c3[2]],
            ];
            let Some(inv) = invert(&mat) else {
                continue;
            };
            let r = mul_vec(&inv, &rhs);
            if r.iter().all(|&v| (-1e-7..=1.0 + 1e-7).contains(&v)) && order_ok(r[0], r[1], r[2]) {
                let candidate = [
                    (cell[0] as f64 + r[0].clamp(0.0, 1.0)) / domain[0],
                    (cell[1] as f64 + r[1].clamp(0.0, 1.0)) / domain[1],
                    (cell[2] as f64 + r[2].clamp(0.0, 1.0)) / domain[2],
                ];
                let fx = self.eval(candidate);
                let err = sq_dist(fx, target);
                if err <= 1e-14 {
                    return Some(candidate);
                }
            }
        }
        None
    }
}

fn sq_dist(a: [f64; 3], b: [f64; 3]) -> f64 {
    let (dx, dy, dz) = (a[0] - b[0], a[1] - b[1], a[2] - b[2]);
    dx * dx + dy * dy + dz * dz
}

/// One processing element of a LUT. Every stage is 3-in 3-out: the profile is
/// RGB and the PCS is XYZ or Lab, and anything else is refused at parse time.
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
    /// True when LittleCMS switches a 16-bit/8-bit `B2A` CLUT to trilinear
    /// interpolation because Lab is its indexer space.
    trilinear: bool,
    /// How the PCS triple is encoded before it meets the first stage.
    pcs_encoding: PcsEncoding,
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
                Stage::Clut(clut) => {
                    if self.trilinear {
                        clut.eval_trilinear(value)
                    } else {
                        clut.eval(value)
                    }
                }
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

/// An inverted 1D curve from an `A2B`/`D2B` pipeline. Uniform tables invert on
/// their original knots without resampling; segmented curves bracket on a
/// monotone sample table over `[0, 1]` and refine on the underlying segments.
enum InvertedTable {
    Uniform(Vec<f64>),
    Segmented { table: Table, samples: Vec<f64> },
}

impl InvertedTable {
    fn from_table(table: Table) -> Result<Self, &'static str> {
        match table {
            Table::Uniform(samples) => {
                validate_monotone_samples(&samples)?;
                Ok(Self::Uniform(samples))
            }
            Table::Segmented(segments) => {
                let table = Table::Segmented(segments);
                let samples: Vec<f64> = (0..CURVE_SAMPLES)
                    .map(|i| table.eval(i as f64 / (CURVE_SAMPLES - 1) as f64))
                    .collect();
                validate_monotone_samples(&samples)?;
                Ok(Self::Segmented { table, samples })
            }
        }
    }

    fn invert(&self, y: f64) -> f64 {
        if !y.is_finite() {
            return 0.0;
        }
        match self {
            Self::Uniform(samples) => invert_sample_slice(samples, y),
            Self::Segmented { table, samples } => {
                let first = samples[0];
                let last = samples[CURVE_SAMPLES - 1];
                if y <= first {
                    if let Table::Segmented(segments) = table {
                        if let Some(x) = segments
                            .first()
                            .and_then(|seg| invert_open_formula(&seg.kind, y))
                        {
                            return x.min(0.0);
                        }
                    }
                    return 0.0;
                }
                if y >= last {
                    if let Table::Segmented(segments) = table {
                        if let Some(x) = segments
                            .last()
                            .and_then(|seg| invert_open_formula(&seg.kind, y))
                        {
                            return x.max(1.0);
                        }
                    }
                    return 1.0;
                }
                let mut low = 0usize;
                let mut high = CURVE_SAMPLES - 1;
                while high - low > 1 {
                    let mid = (low + high) / 2;
                    if samples[mid] <= y {
                        low = mid;
                    } else {
                        high = mid;
                    }
                }
                let scale = 1.0 / (CURVE_SAMPLES - 1) as f64;
                let mut x_lo = low as f64 * scale;
                let mut x_hi = high as f64 * scale;
                let mut f_lo = samples[low] - y;
                let mut f_hi = samples[high] - y;
                let mut x = if f_hi > f_lo {
                    (x_lo + (x_hi - x_lo) * (-f_lo / (f_hi - f_lo))).clamp(x_lo, x_hi)
                } else {
                    0.5 * (x_lo + x_hi)
                };
                for _ in 0..8 {
                    let fx = table.eval(x) - y;
                    if fx.abs() <= 1e-12 || (x_hi - x_lo) <= 1e-12 {
                        break;
                    }
                    if fx <= 0.0 {
                        x_lo = x;
                        f_lo = fx;
                    } else {
                        x_hi = x;
                        f_hi = fx;
                    }
                    x = if (f_hi - f_lo).abs() > 1e-15 {
                        let secant = x_lo - f_lo * (x_hi - x_lo) / (f_hi - f_lo);
                        if secant > x_lo + 0.1 * (x_hi - x_lo)
                            && secant < x_hi - 0.1 * (x_hi - x_lo)
                        {
                            secant
                        } else {
                            0.5 * (x_lo + x_hi)
                        }
                    } else {
                        0.5 * (x_lo + x_hi)
                    };
                }
                x.clamp(0.0, 1.0)
            }
        }
    }
}

fn validate_monotone_samples(samples: &[f64]) -> Result<(), &'static str> {
    if samples.len() < 2 || samples.iter().any(|v| !v.is_finite()) {
        return Err("A2B curve has fewer than two finite samples");
    }
    for pair in samples.windows(2) {
        if pair[1] + 1e-6 < pair[0] {
            return Err("A2B curve is non-monotone and cannot be inverted");
        }
    }
    let first = samples[0];
    let last = *samples.last().unwrap();
    if last - first < 0.05 {
        return Err("A2B curve has insufficient span to invert");
    }
    Ok(())
}

fn invert_sample_slice(samples: &[f64], y: f64) -> f64 {
    let last_idx = samples.len() - 1;
    if y <= samples[0] {
        return 0.0;
    }
    if y >= samples[last_idx] {
        return 1.0;
    }
    let mut low = 0usize;
    let mut high = last_idx;
    while high - low > 1 {
        let mid = (low + high) / 2;
        if samples[mid] <= y {
            low = mid;
        } else {
            high = mid;
        }
    }
    let low_val = samples[low];
    let high_val = samples[high];
    if high_val <= low_val {
        return low as f64 / last_idx as f64;
    }
    let fraction = ((y - low_val) / (high_val - low_val)).clamp(0.0, 1.0);
    (low as f64 + fraction) / last_idx as f64
}

fn invert_open_formula(kind: &SegmentKind, y: f64) -> Option<f64> {
    if let SegmentKind::Formula { kind: 6, params } = kind {
        let (g, a, b, c) = (params[0], params[1], params[2], params[3]);
        if !a.is_finite() || a <= 1e-9 || !g.is_finite() || g <= 0.0 {
            return None;
        }
        if g == 1.0 {
            return Some((y - c - b) / a);
        }
        if y >= c {
            return Some(((y - c).powf(1.0 / g) - b) / a);
        }
    }
    None
}

/// A validated 3D CLUT ready for PCS → device inversion via Newton on the
/// tetrahedral interpolant with bounded bisection as fallback.
struct InvertedClut {
    clut: Clut,
    origin: [f64; 3],
    inv_global_jacobian: Mat3,
    coarse_seeds: Vec<([f64; 3], [f64; 3])>,
    /// The per-channel range the CLUT's nodes cover. A target outside it
    /// has no exact preimage anywhere in the grid — the closed-form cell
    /// solves cannot hit their tolerance — so their seven cells of matrix
    /// inversions are skipped on the out-of-gamut path, which is the path
    /// every narrow-gamut display takes for the colours it cannot show.
    output_min: [f64; 3],
    output_max: [f64; 3],
}

impl InvertedClut {
    fn from_clut(clut: Clut) -> Result<Self, &'static str> {
        let [gx, gy, gz] = clut.grid;
        let (nx, ny, nz) = (gx - 1, gy - 1, gz - 1);
        let stride = [gy * gz * 3, gz * 3, 3];
        let node = |ix: usize, iy: usize, iz: usize| -> [f64; 3] {
            let at = ix * stride[0] + iy * stride[1] + iz * stride[2];
            [clut.table[at], clut.table[at + 1], clut.table[at + 2]]
        };
        let total_cells = nx * ny * nz;
        let mut global_jac = [[0.0; 3]; 3];
        let mut cell_dets = Vec::with_capacity(total_cells);
        for ix in 0..nx {
            for iy in 0..ny {
                for iz in 0..nz {
                    let n000 = node(ix, iy, iz);
                    let n100 = node(ix + 1, iy, iz);
                    let n010 = node(ix, iy + 1, iz);
                    let n001 = node(ix, iy, iz + 1);
                    let n011 = node(ix, iy + 1, iz + 1);
                    let n101 = node(ix + 1, iy, iz + 1);
                    let n110 = node(ix + 1, iy + 1, iz);
                    let n111 = node(ix + 1, iy + 1, iz + 1);
                    let mut dx = [0.0; 3];
                    let mut dy = [0.0; 3];
                    let mut dz = [0.0; 3];
                    for c in 0..3 {
                        dx[c] = ((n100[c] - n000[c]) + (n111[c] - n011[c])) * 0.5 * nx as f64;
                        dy[c] = ((n010[c] - n000[c]) + (n111[c] - n101[c])) * 0.5 * ny as f64;
                        dz[c] = ((n001[c] - n000[c]) + (n111[c] - n110[c])) * 0.5 * nz as f64;
                    }
                    let cell_jac = [
                        [dx[0], dy[0], dz[0]],
                        [dx[1], dy[1], dz[1]],
                        [dx[2], dy[2], dz[2]],
                    ];
                    for r in 0..3 {
                        for c in 0..3 {
                            global_jac[r][c] += cell_jac[r][c] / total_cells as f64;
                        }
                    }
                    cell_dets.push(determinant(&cell_jac));
                }
            }
        }
        // Every input axis must move the output and the global Jacobian must
        // be well-conditioned.
        for col in [0usize, 1, 2] {
            let norm_sq = global_jac[0][col].powi(2)
                + global_jac[1][col].powi(2)
                + global_jac[2][col].powi(2);
            if norm_sq < 0.05 * 0.05 {
                return Err("A2B CLUT axis has negligible span and cannot be inverted");
            }
        }
        let global_det = determinant(&global_jac);
        if !global_det.is_finite() || global_det.abs() < 1e-4 {
            return Err("A2B CLUT global Jacobian is singular");
        }
        let inv_global_jacobian =
            invert(&global_jac).ok_or("A2B CLUT global Jacobian cannot be inverted")?;
        let sign = global_det.signum();
        let mut aligned_cells = 0usize;
        for det in cell_dets {
            if !det.is_finite() || det * sign < -1e-5 {
                return Err("A2B CLUT folds back on itself (orientation reversal)");
            }
            if det * sign > 1e-8 {
                aligned_cells += 1;
            }
        }
        if aligned_cells * 2 < total_cells {
            return Err("A2B CLUT has too many singular cells to invert");
        }

        const SEED_STEPS: usize = 3;
        let mut coarse_seeds = Vec::with_capacity(SEED_STEPS * SEED_STEPS * SEED_STEPS);
        for i in 0..SEED_STEPS {
            for j in 0..SEED_STEPS {
                for k in 0..SEED_STEPS {
                    let x = [
                        i as f64 / (SEED_STEPS - 1) as f64,
                        j as f64 / (SEED_STEPS - 1) as f64,
                        k as f64 / (SEED_STEPS - 1) as f64,
                    ];
                    coarse_seeds.push((x, clut.eval(x)));
                }
            }
        }
        let origin = clut.eval([0.0, 0.0, 0.0]);
        let mut output_min = [f64::INFINITY; 3];
        let mut output_max = [f64::NEG_INFINITY; 3];
        let (nodes, _) = clut.table.as_chunks::<3>();
        for node in nodes {
            for channel in 0..3 {
                output_min[channel] = output_min[channel].min(node[channel]);
                output_max[channel] = output_max[channel].max(node[channel]);
            }
        }
        Ok(Self {
            clut,
            origin,
            inv_global_jacobian,
            coarse_seeds,
            output_min,
            output_max,
        })
    }

    fn invert(&self, target: [f64; 3]) -> [f64; 3] {
        if !target.iter().all(|v| v.is_finite()) {
            return [0.0; 3];
        }
        let domain = [
            (self.clut.grid[0] - 1) as f64,
            (self.clut.grid[1] - 1) as f64,
            (self.clut.grid[2] - 1) as f64,
        ];
        let cell_of = |x: [f64; 3]| -> [usize; 3] {
            [
                ((x[0].clamp(0.0, 1.0) * domain[0]).floor() as usize).min(self.clut.grid[0] - 2),
                ((x[1].clamp(0.0, 1.0) * domain[1]).floor() as usize).min(self.clut.grid[1] - 2),
                ((x[2].clamp(0.0, 1.0) * domain[2]).floor() as usize).min(self.clut.grid[2] - 2),
            ]
        };

        // Seed from the global affine inverse and, when not already close,
        // the 3³ coarse grid.
        let delta0 = [
            target[0] - self.origin[0],
            target[1] - self.origin[1],
            target[2] - self.origin[2],
        ];
        let affine_guess = mul_vec(&self.inv_global_jacobian, &delta0);
        let mut x = [
            finite_clamp(affine_guess[0]),
            finite_clamp(affine_guess[1]),
            finite_clamp(affine_guess[2]),
        ];
        let mut best_err = sq_dist(self.clut.eval(x), target);
        if best_err > 1e-4 {
            for &(seed_x, seed_fx) in &self.coarse_seeds {
                let err = sq_dist(seed_fx, target);
                if err < best_err {
                    best_err = err;
                    x = seed_x;
                }
            }
        }

        // Newton iteration on the piecewise-affine tetrahedral interpolant.
        // Because the interpolant is exact affine inside each tetrahedron,
        // a step that stays inside the target tetrahedron lands on the exact
        // root in one solve.
        for _ in 0..10 {
            if best_err <= 1e-14 {
                return x;
            }
            let (fx, jac) = self.clut.eval_with_jacobian(x);
            let err = sq_dist(fx, target);
            if err < best_err {
                best_err = err;
            }
            if best_err <= 1e-14 {
                return x;
            }
            let residual = [fx[0] - target[0], fx[1] - target[1], fx[2] - target[2]];
            let inv_jac = invert(&jac).unwrap_or(self.inv_global_jacobian);
            let step = mul_vec(&inv_jac, &residual);
            let mut improved = false;
            for alpha in [1.0, 0.5, 0.25, 0.125] {
                let candidate = [
                    finite_clamp(x[0] - alpha * step[0]),
                    finite_clamp(x[1] - alpha * step[1]),
                    finite_clamp(x[2] - alpha * step[2]),
                ];
                let cand_err = sq_dist(self.clut.eval(candidate), target);
                if cand_err + 1e-16 < best_err {
                    x = candidate;
                    best_err = cand_err;
                    improved = true;
                    break;
                }
            }
            if !improved {
                break;
            }
        }

        if best_err <= 1e-9 {
            return x;
        }

        // If Newton stalled on a crease between tetrahedra, check the current
        // cell and its six face-adjacent neighbors in closed form — unless
        // the target is outside the range every node covers, where no cell
        // solve can hit its tolerance and the bisection below is the whole
        // answer.
        let target_in_range = (0..3).all(|channel| {
            target[channel] >= self.output_min[channel] - 1e-6
                && target[channel] <= self.output_max[channel] + 1e-6
        });
        if target_in_range {
            let center = cell_of(x);
            for (dx, dy, dz) in [
                (0, 0, 0),
                (-1, 0, 0),
                (1, 0, 0),
                (0, -1, 0),
                (0, 1, 0),
                (0, 0, -1),
                (0, 0, 1),
            ] {
                let (ix, iy, iz) = (
                    center[0] as isize + dx,
                    center[1] as isize + dy,
                    center[2] as isize + dz,
                );
                if ix >= 0
                    && (ix as usize) < self.clut.grid[0] - 1
                    && iy >= 0
                    && (iy as usize) < self.clut.grid[1] - 1
                    && iz >= 0
                    && (iz as usize) < self.clut.grid[2] - 1
                {
                    if let Some(exact) = self
                        .clut
                        .try_solve_cell([ix as usize, iy as usize, iz as usize], target)
                    {
                        return exact;
                    }
                }
            }
        }

        // Bounded bisection and boundary-projected Gauss-Newton fallback for
        // out-of-gamut PCS values, clamping deterministically to [0, 1]³.
        let mut step_size = 0.25;
        for round in 0..10 {
            if round < 4 {
                let (fx, jac) = self.clut.eval_with_jacobian(x);
                let residual = [fx[0] - target[0], fx[1] - target[1], fx[2] - target[2]];
                if let Some(proj_step) = boundary_projected_step(&jac, &residual, x) {
                    for alpha in [1.0, 0.5, 0.25] {
                        let candidate = [
                            finite_clamp(x[0] + alpha * proj_step[0]),
                            finite_clamp(x[1] + alpha * proj_step[1]),
                            finite_clamp(x[2] + alpha * proj_step[2]),
                        ];
                        let cand_err = sq_dist(self.clut.eval(candidate), target);
                        if cand_err < best_err {
                            x = candidate;
                            best_err = cand_err;
                            break;
                        }
                    }
                }
            }
            for axis in 0..3 {
                for dir in [-1.0, 1.0] {
                    let mut candidate = x;
                    candidate[axis] = finite_clamp(x[axis] + dir * step_size);
                    let cand_err = sq_dist(self.clut.eval(candidate), target);
                    if cand_err < best_err {
                        x = candidate;
                        best_err = cand_err;
                    }
                }
            }
            if best_err <= 1e-9 {
                break;
            }
            step_size *= 0.5;
        }
        [finite_clamp(x[0]), finite_clamp(x[1]), finite_clamp(x[2])]
    }
}

/// Solve a Gauss-Newton step `JᵀJ δ = -Jᵀr` with active-set pinning on any
/// coordinate already sitting on a face of `[0, 1]³` whose gradient pushes
/// outward, so out-of-gamut targets slide smoothly to the closest point on the
/// device gamut boundary.
fn boundary_projected_step(jac: &Mat3, residual: &[f64; 3], x: [f64; 3]) -> Option<[f64; 3]> {
    let mut grad = [0.0; 3];
    let mut jtj = [[0.0; 3]; 3];
    for i in 0..3 {
        grad[i] = jac[0][i] * residual[0] + jac[1][i] * residual[1] + jac[2][i] * residual[2];
        for j in 0..3 {
            jtj[i][j] = jac[0][i] * jac[0][j] + jac[1][i] * jac[1][j] + jac[2][i] * jac[2][j];
        }
        jtj[i][i] += 1e-9;
    }
    for c in 0..3 {
        let pinned_low = x[c] <= 1e-7 && grad[c] >= 0.0;
        let pinned_high = x[c] >= 1.0 - 1e-7 && grad[c] <= 0.0;
        if pinned_low || pinned_high {
            grad[c] = 0.0;
            jtj[c] = [0.0; 3];
            for row in &mut jtj {
                row[c] = 0.0;
            }
            jtj[c][c] = 1.0;
        }
    }
    let inv_jtj = invert(&jtj)?;
    Some(mul_vec(&inv_jtj, &[-grad[0], -grad[1], -grad[2]]))
}

enum InvertedStage {
    Curves([InvertedTable; 3]),
    Matrix(Mat3, [f64; 3]),
    Clut(InvertedClut),
}

struct InvertedLutPipeline {
    /// Stages in reverse (PCS → device) evaluation order.
    stages: Vec<InvertedStage>,
    float: bool,
    pcs_encoding: PcsEncoding,
}

impl InvertedLutPipeline {
    fn from_forward(forward: LutPipeline) -> Result<Self, &'static str> {
        let mut stages = Vec::with_capacity(forward.stages.len());
        for stage in forward.stages.into_iter().rev() {
            let inverted = match stage {
                Stage::Curves([c0, c1, c2]) => InvertedStage::Curves([
                    InvertedTable::from_table(c0)?,
                    InvertedTable::from_table(c1)?,
                    InvertedTable::from_table(c2)?,
                ]),
                Stage::Matrix(matrix, offset) => {
                    let inv_matrix = invert(&matrix)
                        .ok_or("A2B matrix stage is singular and cannot be inverted")?;
                    let inv_offset = mul_vec(&inv_matrix, &[-offset[0], -offset[1], -offset[2]]);
                    InvertedStage::Matrix(inv_matrix, inv_offset)
                }
                Stage::Clut(clut) => InvertedStage::Clut(InvertedClut::from_clut(clut)?),
            };
            stages.push(inverted);
        }
        Ok(Self {
            stages,
            float: forward.float,
            pcs_encoding: forward.pcs_encoding,
        })
    }

    fn eval(&self, input: [f64; 3]) -> [f64; 3] {
        let mut value = input;
        if !self.float {
            value = [
                finite_clamp(value[0]),
                finite_clamp(value[1]),
                finite_clamp(value[2]),
            ];
        }
        for stage in &self.stages {
            value = match stage {
                InvertedStage::Curves(curves) => [
                    curves[0].invert(value[0]),
                    curves[1].invert(value[1]),
                    curves[2].invert(value[2]),
                ],
                InvertedStage::Matrix(inv_matrix, inv_offset) => {
                    let mixed = mul_vec(inv_matrix, &value);
                    [
                        mixed[0] + inv_offset[0],
                        mixed[1] + inv_offset[1],
                        mixed[2] + inv_offset[2],
                    ]
                }
                InvertedStage::Clut(clut) => clut.invert(value),
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

/// Read one PCS → device (`B2A`/`B2D`) tag, whichever of the four LUT types it is.
fn parse_b2a_tag(data: &[u8], pcs: Pcs) -> Option<LutPipeline> {
    if data.len() < 8 {
        return None;
    }
    match &data[0..4] {
        b"mft1" => parse_lut_table(data, false, pcs, true),
        b"mft2" => parse_lut_table(data, true, pcs, true),
        b"mBA " => parse_mba(data, pcs),
        // The MPE type is direction-agnostic — the tag it sits in says which
        // way it runs, and the element order in its position table is the
        // order to evaluate.
        b"mpet" => parse_mpe(data, pcs),
        _ => None,
    }
}

/// Read one device → PCS (`A2B`/`D2B`) tag in its forward stage order, ready
/// for [`InvertedLutPipeline::from_forward`] to validate and invert.
fn parse_a2b_tag(data: &[u8], pcs: Pcs) -> Option<LutPipeline> {
    if data.len() < 8 {
        return None;
    }
    match &data[0..4] {
        b"mft1" => parse_lut_table(data, false, pcs, false),
        b"mft2" => parse_lut_table(data, true, pcs, false),
        b"mAB " => parse_mab(data, pcs),
        b"mpet" => parse_mpe(data, pcs),
        _ => None,
    }
}

/// `mft1` (8-bit) and `mft2` (16-bit): an optional inline matrix, input tables,
/// an optional CLUT, output tables. The matrix comes first — LittleCMS inserts
/// it at the head of the pipeline, ahead of the input tables.
fn parse_lut_table(data: &[u8], wide: bool, pcs: Pcs, is_b2a: bool) -> Option<LutPipeline> {
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
    let pcs_encoding = match (pcs, wide) {
        (Pcs::Xyz, _) => PcsEncoding::Xyz16,
        (Pcs::Lab, true) => PcsEncoding::LabV2,
        (Pcs::Lab, false) => PcsEncoding::LabV4,
    };
    Some(LutPipeline {
        stages,
        float: false,
        trilinear: is_b2a && pcs == Pcs::Lab,
        pcs_encoding,
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
fn parse_mba(data: &[u8], pcs: Pcs) -> Option<LutPipeline> {
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
    let pcs_encoding = match pcs {
        Pcs::Xyz => PcsEncoding::Xyz16,
        Pcs::Lab => PcsEncoding::LabV4,
    };
    Some(LutPipeline {
        stages,
        float: false,
        trilinear: pcs == Pcs::Lab,
        pcs_encoding,
    })
}

/// `mAB ` (v4): the same five element offsets in the header as `mBA `,
/// evaluated in the reverse (device → PCS) stage order: A curves, CLUT, M
/// curves, matrix, B curves.
fn parse_mab(data: &[u8], pcs: Pcs) -> Option<LutPipeline> {
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
    if off_a != 0 {
        stages.push(Stage::Curves(read_embedded_curves(data, off_a)?));
    }
    if off_c != 0 {
        stages.push(Stage::Clut(read_mba_clut(data, off_c)?));
    }
    if off_m != 0 {
        stages.push(Stage::Curves(read_embedded_curves(data, off_m)?));
    }
    if off_mat != 0 {
        stages.push(read_matrix_element(data, off_mat)?);
    }
    if off_b != 0 {
        stages.push(Stage::Curves(read_embedded_curves(data, off_b)?));
    }
    if stages.is_empty() {
        return None;
    }
    let pcs_encoding = match pcs {
        Pcs::Xyz => PcsEncoding::Xyz16,
        Pcs::Lab => PcsEncoding::LabV4,
    };
    Some(LutPipeline {
        stages,
        float: false,
        trilinear: false,
        pcs_encoding,
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
fn parse_mpe(data: &[u8], pcs: Pcs) -> Option<LutPipeline> {
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
    let mut clut_stages = 0usize;
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
            b"clut" => {
                clut_stages += 1;
                if clut_stages > PIPELINE_CLUT_CAP {
                    return None;
                }
                stages.push(Stage::Clut(parse_mpe_clut(element)?));
            }
            b"bACS" | b"eACS" => {}
            _ => return None,
        }
    }
    if stages.is_empty() {
        return None;
    }
    let pcs_encoding = match pcs {
        Pcs::Xyz => PcsEncoding::XyzFloat,
        Pcs::Lab => PcsEncoding::LabFloat,
    };
    Some(LutPipeline {
        stages,
        float: true,
        trilinear: false,
        pcs_encoding,
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

    /// A profile with exactly the tags given, laid out the way a real one is,
    /// and the PCS its header declares. `profile_with_tags` is this with the
    /// `XYZ ` PCS the matrix-shaper tests use.
    fn profile_with_pcs_and_tags(pcs: &[u8; 4], tags: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
        let mut profile = profile_with_tags(tags);
        profile[20..24].copy_from_slice(pcs);
        profile
    }

    /// Eight-bit samples, the width an `mft1` table or CLUT node uses.
    fn words8(values: &[f64]) -> Vec<u8> {
        values
            .iter()
            .map(|v| (v.clamp(0.0, 1.0) * 255.0).round() as u8)
            .collect()
    }

    /// `mft1`: a matrix, 256-entry 8-bit input tables, an 8-bit CLUT and
    /// 256-entry 8-bit output tables.
    fn lut8_tag(
        matrix: &Mat3,
        input: &[Vec<f64>; 3],
        grid: usize,
        clut: &[f64],
        output: &[Vec<f64>; 3],
    ) -> Vec<u8> {
        let mut tag = b"mft1\0\0\0\0".to_vec();
        tag.extend_from_slice(&[3, 3, grid as u8, 0]);
        for row in matrix {
            for value in row {
                tag.extend_from_slice(&fixed(*value));
            }
        }
        for channel in input {
            tag.extend_from_slice(&words8(channel));
        }
        tag.extend_from_slice(&words8(clut));
        for channel in output {
            tag.extend_from_slice(&words8(channel));
        }
        tag
    }

    /// `mAB `: the same five optional elements at the same offsets as `mBA `,
    /// in the device → PCS direction (evaluated A → CLUT → M → matrix → B).
    fn mab_tag(
        b: Option<Vec<u8>>,
        matrix: Option<Vec<u8>>,
        m: Option<Vec<u8>>,
        clut: Option<Vec<u8>>,
        a: Option<Vec<u8>>,
    ) -> Vec<u8> {
        let mut tag = b"mAB \0\0\0\0".to_vec();
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

    /// The inverse of [`lab_f`], for L*a*b* → XYZ under D50.
    fn lab_f_inverse(t: f64) -> f64 {
        if t > 24.0 / 116.0 {
            t * t * t
        } else {
            (108.0 / 841.0) * (t - 16.0 / 116.0)
        }
    }

    /// CIE 1976 `[L*, a*, b*]` to PCS XYZ under D50.
    fn lab_to_pcs_xyz(lab: [f64; 3]) -> [f64; 3] {
        let fy = (lab[0] + 16.0) / 116.0;
        let fx = fy + lab[1] / 500.0;
        let fz = fy - lab[2] / 200.0;
        [
            D50_XYZ[0] * lab_f_inverse(fx),
            D50_XYZ[1] * lab_f_inverse(fy),
            D50_XYZ[2] * lab_f_inverse(fz),
        ]
    }

    /// Normalized v2 16-bit Lab to CIE Lab.
    fn lab_v2_from_normalized(normalized: [f64; 3]) -> [f64; 3] {
        let top = 1.0 / LAB_V2_SCALE;
        [
            normalized[0] * top * 100.0,
            normalized[1] * top * 255.0 - 128.0,
            normalized[2] * top * 255.0 - 128.0,
        ]
    }

    /// CIE Lab to the normalized v2 16-bit Lab an `mft2` tag carries.
    fn lab_v2_normalized(lab: [f64; 3]) -> [f64; 3] {
        [
            (lab[0] / 100.0) * LAB_V2_SCALE,
            ((lab[1] + 128.0) / 255.0) * LAB_V2_SCALE,
            ((lab[2] + 128.0) / 255.0) * LAB_V2_SCALE,
        ]
    }

    /// Normalized v4 16-bit Lab (`mBA `/`mAB `) or v2 8-bit Lab (`mft1`) to
    /// CIE Lab.
    fn lab_v4_from_normalized(normalized: [f64; 3]) -> [f64; 3] {
        [
            normalized[0] * 100.0,
            normalized[1] * 255.0 - 128.0,
            normalized[2] * 255.0 - 128.0,
        ]
    }

    /// CIE Lab to the normalized v4 16-bit Lab an `mBA `/`mAB ` tag carries.
    fn lab_v4_normalized(lab: [f64; 3]) -> [f64; 3] {
        [
            lab[0] / 100.0,
            (lab[1] + 128.0) / 255.0,
            (lab[2] + 128.0) / 255.0,
        ]
    }

    /// Linear display light from PCS XYZ for the sRGB-primary display.
    fn linear_from_pcs(xyz: [f64; 3]) -> [f64; 3] {
        let from_pcs = invert(&SRGB_D50).expect("sRGB primaries invert");
        mul_vec(&from_pcs, &xyz)
    }

    /// PCS XYZ from linear display light.
    fn pcs_from_linear(linear: [f64; 3]) -> [f64; 3] {
        mul_vec(&SRGB_D50, &linear)
    }

    fn gamma_encode_tables() -> [Vec<f64>; 3] {
        [
            gamma_table(256, 2.2),
            gamma_table(256, 2.2),
            gamma_table(256, 2.2),
        ]
    }

    fn gamma_decode_tables() -> [Vec<f64>; 3] {
        [
            gamma_table(256, 1.0 / 2.2),
            gamma_table(256, 1.0 / 2.2),
            gamma_table(256, 1.0 / 2.2),
        ]
    }

    fn identity_tables() -> [Vec<f64>; 3] {
        [
            identity_table(256),
            identity_table(256),
            identity_table(256),
        ]
    }

    fn identity_curve() -> Vec<u8> {
        b"curv\0\0\0\0\0\0\0\0".to_vec()
    }

    fn identity_curve_set() -> Vec<u8> {
        curves_blob(&[identity_curve(), identity_curve(), identity_curve()])
    }

    fn gamma_curve_set(gamma: f64) -> Vec<u8> {
        curves_blob(&[curv_gamma(gamma), curv_gamma(gamma), curv_gamma(gamma)])
    }

    fn cvst_formula_set(kind: u16, params: &[f32]) -> Vec<u8> {
        let curve = curf_formula(kind, params);
        cvst_element(&[curve.clone(), curve.clone(), curve])
    }

    /// The gamma-2.2 sRGB-primary display as a v2 16-bit Lab `B2A` table:
    /// identity Lab input tables, a 17³ CLUT of v2-encoded Lab to linear
    /// light, gamma-2.2 output tables.
    fn lut16_lab_b2a_profile() -> Vec<u8> {
        let clut = clut_nodes(17, |normalized| {
            linear_from_pcs(lab_to_pcs_xyz(lab_v2_from_normalized(normalized)))
        });
        let tag = lut16_tag(
            &IDENTITY,
            &identity_tables(),
            17,
            &clut,
            &gamma_encode_tables(),
        );
        profile_with_pcs_and_tags(b"Lab ", &[(b"B2A1", tag)])
    }

    /// The same display as a v4 `mBA ` with a Lab PCS: identity B curves, a
    /// 17³ CLUT of v4-encoded Lab to linear light, gamma-2.2 A curves.
    fn mba_lab_b2a_profile() -> Vec<u8> {
        let clut = clut_nodes(17, |normalized| {
            linear_from_pcs(lab_to_pcs_xyz(lab_v4_from_normalized(normalized)))
        });
        let tag = mba_tag(
            Some(identity_curve_set()),
            None,
            None,
            Some(mba_clut_bytes([17, 17, 17], &clut)),
            Some(gamma_curve_set(1.0 / 2.2)),
        );
        profile_with_pcs_and_tags(b"Lab ", &[(b"B2A1", tag)])
    }

    /// The same display as a v4 float Lab `B2D` tag. LittleCMS hands an
    /// `mpet` tag raw `[L*, a*, b*]`, so the first element inside the tag is
    /// the matrix that normalizes them; the CLUT is then the same
    /// v4-normalized Lab → linear light table, and gamma-2.2 A curves encode
    /// the device values.
    fn mpe_lab_b2d_profile() -> Vec<u8> {
        let clut = clut_nodes(17, |normalized| {
            linear_from_pcs(lab_to_pcs_xyz(lab_v4_from_normalized(normalized)))
        });
        let normalize = matf_element(
            &[
                [1.0 / 100.0, 0.0, 0.0],
                [0.0, 1.0 / 255.0, 0.0],
                [0.0, 0.0, 1.0 / 255.0],
            ],
            &[0.0, 128.0 / 255.0, 128.0 / 255.0],
        );
        let tag = mpe_tag(&[
            cvst_formula_set(0, &[1.0, 1.0, 0.0, 0.0]),
            normalize,
            clut_element([17, 17, 17], &clut),
            cvst_formula_set(0, &[1.0 / 2.2, 1.0, 0.0, 0.0]),
        ]);
        profile_with_pcs_and_tags(b"Lab ", &[(b"B2D1", tag)])
    }

    /// The display's device → PCS direction as a v2 8-bit XYZ table (`mft1`):
    /// the inline 16.16 fixed-point matrix carries the primaries and 1.15 PCS
    /// scale, while the 256-entry 8-bit input tables, 8-bit CLUT and 256-entry
    /// 8-bit output tables are the identity, so all four `mft1` stages are
    /// inverted without compounding two stages of 8-bit quantization.
    fn lut8_xyz_a2b_profile() -> Vec<u8> {
        let mut matrix = SRGB_D50;
        for row in &mut matrix {
            for value in row {
                *value *= XYZ_PCS_SCALE;
            }
        }
        let clut = clut_nodes(16, |xyz| xyz);
        let tag = lut8_tag(&matrix, &identity_tables(), 16, &clut, &identity_tables());
        profile_with_pcs_and_tags(b"XYZ ", &[(b"A2B1", tag)])
    }

    /// The same table at 16 bits, with a 17³ CLUT.
    fn lut16_xyz_a2b_profile() -> Vec<u8> {
        let clut = clut_nodes(17, |linear| {
            let xyz = pcs_from_linear(linear);
            [
                xyz[0] * XYZ_PCS_SCALE,
                xyz[1] * XYZ_PCS_SCALE,
                xyz[2] * XYZ_PCS_SCALE,
            ]
        });
        let tag = lut16_tag(
            &IDENTITY,
            &gamma_decode_tables(),
            17,
            &clut,
            &identity_tables(),
        );
        profile_with_pcs_and_tags(b"XYZ ", &[(b"A2B1", tag)])
    }

    /// The same display as a v4 `mAB ` with an XYZ PCS: gamma-2.2 A curves, a
    /// 17³ CLUT of linear light to 1.15 fixed XYZ, identity B curves.
    fn mab_xyz_a2b_profile() -> Vec<u8> {
        let clut = clut_nodes(17, |linear| {
            let xyz = pcs_from_linear(linear);
            [
                xyz[0] * XYZ_PCS_SCALE,
                xyz[1] * XYZ_PCS_SCALE,
                xyz[2] * XYZ_PCS_SCALE,
            ]
        });
        let tag = mab_tag(
            Some(identity_curve_set()),
            None,
            None,
            Some(mba_clut_bytes([17, 17, 17], &clut)),
            Some(gamma_curve_set(2.2)),
        );
        profile_with_pcs_and_tags(b"XYZ ", &[(b"A2B1", tag)])
    }

    /// The same display as a v2 16-bit Lab `A2B`: identity input tables, a 17³
    /// CLUT of gamma-2.2 device RGB to v2-encoded Lab, identity output tables.
    /// Sampling the CLUT in device space rather than linear light keeps the
    /// `L* ∝ (x^2.2)^(1/3) = x^0.733` curve smooth near black, the way a
    /// profiler's device-grid measurement table is laid out.
    fn lut16_lab_a2b_profile() -> Vec<u8> {
        let clut = clut_nodes(17, |device| {
            let linear = [
                device[0].powf(2.2),
                device[1].powf(2.2),
                device[2].powf(2.2),
            ];
            lab_v2_normalized(xyz_to_lab(pcs_from_linear(linear)))
        });
        let tag = lut16_tag(&IDENTITY, &identity_tables(), 17, &clut, &identity_tables());
        profile_with_pcs_and_tags(b"Lab ", &[(b"A2B1", tag)])
    }

    /// The same as a v4 `mAB ` with a Lab PCS: identity A curves, a 17³ CLUT
    /// of gamma-2.2 device RGB to v4-encoded Lab, identity B curves.
    fn mab_lab_a2b_profile() -> Vec<u8> {
        let clut = clut_nodes(17, |device| {
            let linear = [
                device[0].powf(2.2),
                device[1].powf(2.2),
                device[2].powf(2.2),
            ];
            lab_v4_normalized(xyz_to_lab(pcs_from_linear(linear)))
        });
        let tag = mab_tag(
            Some(identity_curve_set()),
            None,
            None,
            Some(mba_clut_bytes([17, 17, 17], &clut)),
            Some(identity_curve_set()),
        );
        profile_with_pcs_and_tags(b"Lab ", &[(b"A2B1", tag)])
    }

    /// The same as a v4 float Lab `D2B` tag: identity A curves, a 17³ CLUT of
    /// gamma-2.2 device RGB to v4-normalized Lab, then the matrix that turns
    /// the normalized Lab back into the raw `[L*, a*, b*]` LittleCMS expects
    /// at the tag boundary, and identity B curves.
    fn mpe_lab_d2b_profile() -> Vec<u8> {
        let clut = clut_nodes(17, |device| {
            let linear = [
                device[0].powf(2.2),
                device[1].powf(2.2),
                device[2].powf(2.2),
            ];
            lab_v4_normalized(xyz_to_lab(pcs_from_linear(linear)))
        });
        let denormalize = matf_element(
            &[[100.0, 0.0, 0.0], [0.0, 255.0, 0.0], [0.0, 0.0, 255.0]],
            &[0.0, -128.0, -128.0],
        );
        let tag = mpe_tag(&[
            cvst_formula_set(0, &[1.0, 1.0, 0.0, 0.0]),
            clut_element([17, 17, 17], &clut),
            denormalize,
            cvst_formula_set(0, &[1.0, 1.0, 0.0, 0.0]),
        ]);
        profile_with_pcs_and_tags(b"Lab ", &[(b"D2B1", tag)])
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
            ("lut16_lab_b2a.icc", lut16_lab_b2a_profile()),
            ("mba_lab_b2a.icc", mba_lab_b2a_profile()),
            ("mpe_lab_b2d.icc", mpe_lab_b2d_profile()),
            ("lut8_xyz_a2b.icc", lut8_xyz_a2b_profile()),
            ("lut16_xyz_a2b.icc", lut16_xyz_a2b_profile()),
            ("mab_xyz_a2b.icc", mab_xyz_a2b_profile()),
            ("lut16_lab_a2b.icc", lut16_lab_a2b_profile()),
            ("mab_lab_a2b.icc", mab_lab_a2b_profile()),
            ("mpe_lab_d2b.icc", mpe_lab_d2b_profile()),
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

    // -----------------------------------------------------------------------
    // Lab PCS and inverted A2B/D2B cross-checks against LittleCMS 2.19
    // -----------------------------------------------------------------------

    /// The CIE 2000 colour difference (ΔE00), with parametric weights
    /// kL = kC = kH = 1, following Sharma, Wu and Dalal (2005).
    fn ciede2000(lab1: [f64; 3], lab2: [f64; 3]) -> f64 {
        let [l1, a1, b1] = lab1;
        let [l2, a2, b2] = lab2;
        let c1 = (a1 * a1 + b1 * b1).sqrt();
        let c2 = (a2 * a2 + b2 * b2).sqrt();
        let c_bar = 0.5 * (c1 + c2);
        const POW25_7: f64 = 6_103_515_625.0;
        let c_bar7 = c_bar.powi(7);
        let g = 0.5 * (1.0 - (c_bar7 / (c_bar7 + POW25_7)).sqrt());
        let a1p = (1.0 + g) * a1;
        let a2p = (1.0 + g) * a2;
        let c1p = (a1p * a1p + b1 * b1).sqrt();
        let c2p = (a2p * a2p + b2 * b2).sqrt();
        let h1p = b1.atan2(a1p).to_degrees().rem_euclid(360.0);
        let h2p = b2.atan2(a2p).to_degrees().rem_euclid(360.0);

        let delta_lp = l2 - l1;
        let delta_cp = c2p - c1p;
        let hue_delta = h2p - h1p;
        let delta_hp = if c1p * c2p == 0.0 {
            0.0
        } else if hue_delta.abs() <= 180.0 {
            hue_delta
        } else if hue_delta > 180.0 {
            hue_delta - 360.0
        } else {
            hue_delta + 360.0
        };
        let delta_hp_big = 2.0 * (c1p * c2p).sqrt() * (delta_hp / 2.0).to_radians().sin();

        let l_bar = 0.5 * (l1 + l2);
        let c_bar_p = 0.5 * (c1p + c2p);
        let h_bar_p = if c1p * c2p == 0.0 {
            h1p + h2p
        } else if (h1p - h2p).abs() <= 180.0 {
            0.5 * (h1p + h2p)
        } else if h1p + h2p < 360.0 {
            0.5 * (h1p + h2p + 360.0)
        } else {
            0.5 * (h1p + h2p - 360.0)
        };

        let t = 1.0 - 0.17 * (h_bar_p - 30.0).to_radians().cos()
            + 0.24 * (2.0 * h_bar_p).to_radians().cos()
            + 0.32 * (3.0 * h_bar_p + 6.0).to_radians().cos()
            - 0.20 * (4.0 * h_bar_p - 63.0).to_radians().cos();
        let delta_theta = 30.0 * (-((h_bar_p - 275.0) / 25.0).powi(2)).exp();
        let c_bar_p7 = c_bar_p.powi(7);
        let r_c = 2.0 * (c_bar_p7 / (c_bar_p7 + POW25_7)).sqrt();
        let l_offset = (l_bar - 50.0).powi(2);
        let s_l = 1.0 + 0.015 * l_offset / (20.0 + l_offset).sqrt();
        let s_c = 1.0 + 0.045 * c_bar_p;
        let s_h = 1.0 + 0.015 * c_bar_p * t;
        let r_t = -(2.0 * delta_theta).to_radians().sin() * r_c;

        let l_term = delta_lp / s_l;
        let c_term = delta_cp / s_c;
        let h_term = delta_hp_big / s_h;
        (l_term * l_term + c_term * c_term + h_term * h_term + r_t * c_term * h_term).sqrt()
    }

    fn gamma22_display_to_lab_d50(rgb: [f64; 3]) -> [f64; 3] {
        let linear = [
            finite_clamp(rgb[0]).powf(2.2),
            finite_clamp(rgb[1]).powf(2.2),
            finite_clamp(rgb[2]).powf(2.2),
        ];
        xyz_to_lab(pcs_from_linear(linear))
    }

    fn linear_display_to_lab_d50(rgb: [f64; 3]) -> [f64; 3] {
        let linear = [
            finite_clamp(rgb[0]),
            finite_clamp(rgb[1]),
            finite_clamp(rgb[2]),
        ];
        xyz_to_lab(pcs_from_linear(linear))
    }

    fn assert_matches_lcms_16bit(
        transform: &DisplayTransform,
        expected: &[([f64; 3], [u16; 3])],
        display_to_lab: fn([f64; 3]) -> [f64; 3],
    ) {
        for &(input, wanted_u16) in expected {
            let wanted = [
                f64::from(wanted_u16[0]) / 65535.0,
                f64::from(wanted_u16[1]) / 65535.0,
                f64::from(wanted_u16[2]) / 65535.0,
            ];
            let mapped = transform.map(input);
            for channel in 0..3 {
                let err = (mapped[channel] - wanted[channel]).abs();
                assert!(
                    err <= 1.0 / 255.0,
                    "{input:?} became {mapped:?}, LittleCMS says {wanted:?} (err {err})"
                );
            }
            let de = ciede2000(display_to_lab(mapped), display_to_lab(wanted));
            assert!(
                de <= 0.5,
                "{input:?} became {mapped:?}, LittleCMS says {wanted:?} (ΔE00 {de})"
            );
        }
    }

    /// v2 16-bit legacy Lab (`B2A1` `mft2`): generated with
    /// ```sh
    /// nix shell nixpkgs#lcms2 -c bash -c '
    ///   printf "255 0 0\n0 255 0\n0 0 255\n128 128 128\n51 102 153\n" |
    ///     transicc -i"*sRGB" -o/tmp/icc-lut-fixtures/lut16_lab_b2a.icc -t1 -w -e -n
    /// '
    /// ```
    #[test]
    fn a_lut16_lab_b2a_display_matches_lcms2() {
        let transform =
            DisplayTransform::for_profile(&lut16_lab_b2a_profile()).expect("a transform");
        let expected = [
            ([1.0, 0.0, 0.0], [64278, 6961, 5850]),
            ([0.0, 1.0, 0.0], [7220, 65454, 5294]),
            ([0.0, 0.0, 1.0], [10268, 5964, 64533]),
            (
                [0.501_960_8, 0.501_960_8, 0.501_960_8],
                [32736, 32735, 32729],
            ),
            ([0.2, 0.4, 0.6], [14872, 26255, 39106]),
        ];
        assert_matches_lcms_16bit(&transform, &expected, gamma22_display_to_lab_d50);
    }

    /// v4 16-bit Lab (`B2A1` `mBA `): generated with
    /// ```sh
    /// nix shell nixpkgs#lcms2 -c bash -c '
    ///   printf "255 0 0\n0 255 0\n0 0 255\n128 128 128\n51 102 153\n" |
    ///     transicc -i"*sRGB" -o/tmp/icc-lut-fixtures/mba_lab_b2a.icc -t1 -w -e -n
    /// '
    /// ```
    #[test]
    fn an_mba_lab_b2a_display_matches_lcms2() {
        let transform = DisplayTransform::for_profile(&mba_lab_b2a_profile()).expect("a transform");
        let expected = [
            ([1.0, 0.0, 0.0], [64354, 7518, 6097]),
            ([0.0, 1.0, 0.0], [8497, 65293, 6886]),
            ([0.0, 0.0, 1.0], [10789, 6307, 64574]),
            (
                [0.501_960_8, 0.501_960_8, 0.501_960_8],
                [32807, 32802, 32824],
            ),
            ([0.2, 0.4, 0.6], [14955, 26328, 39151]),
        ];
        assert_matches_lcms_16bit(&transform, &expected, gamma22_display_to_lab_d50);
    }

    /// v4 float Lab (`B2D1` `mpet`): generated with
    /// ```sh
    /// nix shell nixpkgs#lcms2 -c bash -c '
    ///   printf "255 0 0\n0 255 0\n0 0 255\n128 128 128\n51 102 153\n" |
    ///     transicc -i"*sRGB" -o/tmp/icc-lut-fixtures/mpe_lab_b2d.icc -t1 -w -e -n
    /// '
    /// ```
    #[test]
    fn an_mpe_lab_b2d_display_matches_lcms2() {
        let transform = DisplayTransform::for_profile(&mpe_lab_b2d_profile()).expect("a transform");
        let expected = [
            ([1.0, 0.0, 0.0], [65535, 1825, 2592]),
            ([0.0, 1.0, 0.0], [3683, 65534, 2160]),
            ([0.0, 0.0, 1.0], [4979, 0, 65535]),
            (
                [0.501_960_8, 0.501_960_8, 0.501_960_8],
                [32759, 32724, 32738],
            ),
            ([0.2, 0.4, 0.6], [14672, 26183, 39039]),
        ];
        assert_matches_lcms_16bit(&transform, &expected, gamma22_display_to_lab_d50);
    }

    /// v2 8-bit XYZ (`A2B1` `mft1`, inverted): LittleCMS reads the `A2B1` tag
    /// in the forward direction (`device → *sRGB`) via
    /// ```sh
    /// nix shell nixpkgs#lcms2 -c bash -c '
    ///   printf "178.5 76.5 76.5\n76.5 178.5 76.5\n76.5 76.5 178.5\n128 128 128\n51 102 153\n" |
    ///     transicc -i/tmp/icc-lut-fixtures/lut8_xyz_a2b.icc -o"*sRGB" -t1 -w -e -n
    /// '
    /// ```
    /// and the inverted transform recovers each device triple from LittleCMS's
    /// 16-bit sRGB output within ≤ 1/255.
    #[test]
    fn a_lut8_xyz_a2b_display_matches_lcms2() {
        let transform =
            DisplayTransform::for_profile(&lut8_xyz_a2b_profile()).expect("a transform");
        let expected = [
            (
                [55989.0 / 65535.0, 38261.0 / 65535.0, 38270.0 / 65535.0],
                [45875, 19661, 19661],
            ),
            (
                [38257.0 / 65535.0, 55987.0 / 65535.0, 38270.0 / 65535.0],
                [19661, 45875, 19661],
            ),
            (
                [38264.0 / 65535.0, 38260.0 / 65535.0, 55997.0 / 65535.0],
                [19661, 19661, 45875],
            ),
            (
                [48276.0 / 65535.0, 48275.0 / 65535.0, 48285.0 / 65535.0],
                [32896, 32896, 32896],
            ),
            (
                [31750.0 / 65535.0, 43592.0 / 65535.0, 52289.0 / 65535.0],
                [13107, 26214, 39321],
            ),
        ];
        assert_matches_lcms_16bit(&transform, &expected, linear_display_to_lab_d50);
    }

    /// v2 16-bit XYZ (`A2B1` `mft2`, inverted): reference pairs generated with
    /// ```sh
    /// nix shell nixpkgs#lcms2 -c bash -c '
    ///   printf "178.5 76.5 76.5\n76.5 178.5 76.5\n76.5 76.5 178.5\n128 128 128\n51 102 153\n" |
    ///     transicc -i/tmp/icc-lut-fixtures/lut16_xyz_a2b.icc -o"*sRGB" -t1 -w -e -n
    /// '
    /// ```
    #[test]
    fn a_lut16_xyz_a2b_display_matches_lcms2() {
        let transform =
            DisplayTransform::for_profile(&lut16_xyz_a2b_profile()).expect("a transform");
        let expected = [
            (
                [46254.0 / 65535.0, 19327.0 / 65535.0, 19332.0 / 65535.0],
                [45875, 19661, 19661],
            ),
            (
                [19313.0 / 65535.0, 46253.0 / 65535.0, 19336.0 / 65535.0],
                [19661, 45875, 19661],
            ),
            (
                [19328.0 / 65535.0, 19326.0 / 65535.0, 46261.0 / 65535.0],
                [19661, 19661, 45875],
            ),
            (
                [33153.0 / 65535.0, 33151.0 / 65535.0, 33160.0 / 65535.0],
                [32896, 32896, 32896],
            ),
            (
                [12207.0 / 65535.0, 26244.0 / 65535.0, 39689.0 / 65535.0],
                [13107, 26214, 39321],
            ),
        ];
        assert_matches_lcms_16bit(&transform, &expected, gamma22_display_to_lab_d50);
    }

    /// v4 16-bit XYZ (`A2B1` `mAB `, inverted): reference pairs generated with
    /// ```sh
    /// nix shell nixpkgs#lcms2 -c bash -c '
    ///   printf "178.5 76.5 76.5\n76.5 178.5 76.5\n76.5 76.5 178.5\n128 128 128\n51 102 153\n" |
    ///     transicc -i/tmp/icc-lut-fixtures/mab_xyz_a2b.icc -o"*sRGB" -t1 -w -e -n
    /// '
    /// ```
    #[test]
    fn an_mab_xyz_a2b_display_matches_lcms2() {
        let transform = DisplayTransform::for_profile(&mab_xyz_a2b_profile()).expect("a transform");
        let expected = [
            (
                [46262.0 / 65535.0, 19331.0 / 65535.0, 19337.0 / 65535.0],
                [45875, 19661, 19661],
            ),
            (
                [19323.0 / 65535.0, 46260.0 / 65535.0, 19339.0 / 65535.0],
                [19661, 45875, 19661],
            ),
            (
                [19334.0 / 65535.0, 19334.0 / 65535.0, 46267.0 / 65535.0],
                [19661, 19661, 45875],
            ),
            (
                [33163.0 / 65535.0, 33159.0 / 65535.0, 33167.0 / 65535.0],
                [32896, 32896, 32896],
            ),
            (
                [12203.0 / 65535.0, 26256.0 / 65535.0, 39698.0 / 65535.0],
                [13107, 26214, 39321],
            ),
        ];
        assert_matches_lcms_16bit(&transform, &expected, gamma22_display_to_lab_d50);
    }

    /// v2 16-bit legacy Lab (`A2B1` `mft2`, inverted): reference pairs
    /// generated with
    /// ```sh
    /// nix shell nixpkgs#lcms2 -c bash -c '
    ///   printf "255 0 0\n0 255 0\n0 0 255\n128 128 128\n51 102 153\n" |
    ///     transicc -i/tmp/icc-lut-fixtures/lut16_lab_a2b.icc -o"*sRGB" -t1 -w -e -n
    /// '
    /// ```
    #[test]
    fn a_lut16_lab_a2b_display_matches_lcms2() {
        let transform =
            DisplayTransform::for_profile(&lut16_lab_a2b_profile()).expect("a transform");
        let expected = [
            ([1.0, 0.0, 0.0], [65535, 0, 0]),
            ([0.0, 1.0, 0.0], [0, 65535, 0]),
            ([0.0, 0.0, 1.0], [0, 0, 65535]),
            (
                [33150.0 / 65535.0, 33151.0 / 65535.0, 33158.0 / 65535.0],
                [32896, 32896, 32896],
            ),
            (
                [12317.0 / 65535.0, 26228.0 / 65535.0, 39680.0 / 65535.0],
                [13107, 26214, 39321],
            ),
        ];
        assert_matches_lcms_16bit(&transform, &expected, gamma22_display_to_lab_d50);
    }

    /// v4 16-bit Lab (`A2B1` `mAB `, inverted): reference pairs generated with
    /// ```sh
    /// nix shell nixpkgs#lcms2 -c bash -c '
    ///   printf "255 0 0\n0 255 0\n0 0 255\n128 128 128\n51 102 153\n" |
    ///     transicc -i/tmp/icc-lut-fixtures/mab_lab_a2b.icc -o"*sRGB" -t1 -w -e -n
    /// '
    /// ```
    #[test]
    fn an_mab_lab_a2b_display_matches_lcms2() {
        let transform = DisplayTransform::for_profile(&mab_lab_a2b_profile()).expect("a transform");
        let expected = [
            ([1.0, 0.0, 0.0], [65535, 0, 0]),
            ([0.0, 1.0, 0.0], [0, 65535, 0]),
            ([0.0, 0.0, 1.0], [0, 0, 65535]),
            (
                [33150.0 / 65535.0, 33150.0 / 65535.0, 33157.0 / 65535.0],
                [32896, 32896, 32896],
            ),
            (
                [12315.0 / 65535.0, 26229.0 / 65535.0, 39682.0 / 65535.0],
                [13107, 26214, 39321],
            ),
        ];
        assert_matches_lcms_16bit(&transform, &expected, gamma22_display_to_lab_d50);
    }

    /// v4 float Lab (`D2B1` `mpet`, inverted): reference pairs generated with
    /// ```sh
    /// nix shell nixpkgs#lcms2 -c bash -c '
    ///   printf "255 0 0\n0 255 0\n0 0 255\n128 128 128\n51 102 153\n" |
    ///     transicc -i/tmp/icc-lut-fixtures/mpe_lab_d2b.icc -o"*sRGB" -t1 -w -e -n
    /// '
    /// ```
    #[test]
    fn an_mpe_lab_d2b_display_matches_lcms2() {
        let transform = DisplayTransform::for_profile(&mpe_lab_d2b_profile()).expect("a transform");
        let expected = [
            ([1.0, 0.0, 0.0], [65535, 0, 0]),
            ([0.0, 1.0, 0.0], [0, 65535, 0]),
            ([0.0, 0.0, 1.0], [0, 0, 65535]),
            (
                [33149.0 / 65535.0, 33150.0 / 65535.0, 33157.0 / 65535.0],
                [32896, 32896, 32896],
            ),
            (
                [12316.0 / 65535.0, 26228.0 / 65535.0, 39681.0 / 65535.0],
                [13107, 26214, 39321],
            ),
        ];
        assert_matches_lcms_16bit(&transform, &expected, gamma22_display_to_lab_d50);
    }

    /// Out-of-gamut clamping on a narrow-gamut `A2B1` display profile whose
    /// primaries cover only an interior 30% slice of sRGB around the D50
    /// neutral axis: saturated sRGB primaries and out-of-range inputs clamp
    /// deterministically to `[0, 1]` on the device gamut boundary with no NaN
    /// or panic.
    #[test]
    fn a_narrow_gamut_a2b_clamps_out_of_gamut_colours() {
        let mut narrow_primaries = [[0.0; 3]; 3];
        for row in 0..3 {
            for col in 0..3 {
                narrow_primaries[row][col] = 0.3 * SRGB_D50[row][col] + 0.7 * D50_XYZ[row] / 3.0;
            }
        }
        let clut = clut_nodes(9, |linear| {
            let xyz = mul_vec(&narrow_primaries, &linear);
            [
                xyz[0] * XYZ_PCS_SCALE,
                xyz[1] * XYZ_PCS_SCALE,
                xyz[2] * XYZ_PCS_SCALE,
            ]
        });
        let tag = mab_tag(
            Some(identity_curve_set()),
            None,
            None,
            Some(mba_clut_bytes([9, 9, 9], &clut)),
            Some(gamma_curve_set(2.2)),
        );
        let profile = profile_with_pcs_and_tags(b"XYZ ", &[(b"A2B1", tag)]);
        let transform = DisplayTransform::for_profile(&profile).expect("a transform");

        for input in [
            [1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [0.0, 0.0, 1.0],
            [1.0, 1.0, 0.0],
            [0.0, 1.0, 1.0],
            [1.0, 0.0, 1.0],
            [1.5, -0.5, 0.5],
            [f64::NAN, 0.5, 0.5],
            [f64::INFINITY, 0.0, 0.0],
        ] {
            let mapped = transform.map(input);
            let again = transform.map(input);
            assert_eq!(mapped, again, "{input:?} was not deterministic");
            assert!(
                mapped
                    .iter()
                    .all(|v| v.is_finite() && (0.0..=1.0).contains(v)),
                "{input:?} became {mapped:?}"
            );
        }
        // Saturated sRGB red lands on the upper red/lower green-blue boundary
        // of the narrow display gamut.
        let red = transform.map([1.0, 0.0, 0.0]);
        assert!(red[0] > 0.8 && red[1] < 1e-4 && red[2] < 1e-4, "{red:?}");

        let lut = transform.lut();
        assert_eq!(lut.len(), LUT_SIZE * LUT_SIZE * LUT_SIZE);
        assert!(lut.iter().all(|e| e[3] == 1.0
            && e[0..3]
                .iter()
                .all(|v| v.is_finite() && (0.0..=1.0).contains(v))));
    }

    /// A present-but-unreadable `B2A1` tag refuses the profile rather than
    /// falling back to a valid `A2B1` tag, and an unreadable `A2B1` refuses
    /// rather than falling back to `A2B0`.
    #[test]
    fn an_unreadable_higher_priority_lut_tag_does_not_fall_back() {
        let valid_a2b = mab_xyz_a2b_profile();
        assert!(DisplayTransform::for_profile(&valid_a2b).is_some());

        let a2b_tag = lookup_tag(&valid_a2b, b"A2B1");
        let TagLookup::Present(a2b_bytes) = a2b_tag else {
            panic!("expected A2B1 tag");
        };
        let mut corrupt_b2a = a2b_bytes.to_vec();
        corrupt_b2a[0..4].copy_from_slice(b"mBA ");
        corrupt_b2a.truncate(20);

        let with_corrupt_b2a = profile_with_pcs_and_tags(
            b"XYZ ",
            &[(b"B2A1", corrupt_b2a), (b"A2B1", a2b_bytes.to_vec())],
        );
        assert!(DisplayTransform::for_profile(&with_corrupt_b2a).is_none());

        let mut corrupt_a2b1 = a2b_bytes.to_vec();
        corrupt_a2b1.truncate(20);
        let with_corrupt_a2b1 = profile_with_pcs_and_tags(
            b"XYZ ",
            &[(b"A2B1", corrupt_a2b1), (b"A2B0", a2b_bytes.to_vec())],
        );
        assert!(DisplayTransform::for_profile(&with_corrupt_a2b1).is_none());
    }

    /// Non-invertible `A2B` stages — a constant or folding CLUT, a backwards
    /// or flat curve, a singular matrix — and non-RGB device spaces are
    /// refused.
    #[test]
    fn non_invertible_a2b_stages_and_non_rgb_device_spaces_are_refused() {
        // Constant CLUT.
        let constant_clut = clut_nodes(3, |_| [0.5, 0.5, 0.5]);
        let constant_tag = mab_tag(
            Some(identity_curve_set()),
            None,
            None,
            Some(mba_clut_bytes([3, 3, 3], &constant_clut)),
            Some(gamma_curve_set(2.2)),
        );
        assert!(DisplayTransform::for_profile(&profile_with_pcs_and_tags(
            b"XYZ ",
            &[(b"A2B1", constant_tag)],
        ))
        .is_none());

        // Folding CLUT (first axis reverses direction past 0.5).
        let folding_clut = clut_nodes(5, |rgb| {
            [
                if rgb[0] <= 0.5 { rgb[0] } else { 1.0 - rgb[0] },
                rgb[1],
                rgb[2],
            ]
        });
        let folding_tag = mab_tag(
            Some(identity_curve_set()),
            None,
            None,
            Some(mba_clut_bytes([5, 5, 5], &folding_clut)),
            Some(gamma_curve_set(2.2)),
        );
        assert!(DisplayTransform::for_profile(&profile_with_pcs_and_tags(
            b"XYZ ",
            &[(b"A2B1", folding_tag)],
        ))
        .is_none());

        // Backwards curve in A2B.
        let mut backwards_curv = b"curv\0\0\0\0".to_vec();
        backwards_curv.extend_from_slice(&3u32.to_be_bytes());
        for v in [0u16, 50_000, 20_000] {
            backwards_curv.extend_from_slice(&v.to_be_bytes());
        }
        let backwards_tag = mab_tag(
            Some(curves_blob(&[
                backwards_curv,
                identity_curve(),
                identity_curve(),
            ])),
            None,
            None,
            None,
            None,
        );
        assert!(DisplayTransform::for_profile(&profile_with_pcs_and_tags(
            b"XYZ ",
            &[(b"A2B1", backwards_tag)],
        ))
        .is_none());

        // Singular matrix in A2B.
        let singular = [[1.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]];
        let singular_tag = mab_tag(
            Some(identity_curve_set()),
            Some(matrix_element_bytes(&singular, &[0.0; 3])),
            None,
            None,
            None,
        );
        assert!(DisplayTransform::for_profile(&profile_with_pcs_and_tags(
            b"XYZ ",
            &[(b"A2B1", singular_tag)],
        ))
        .is_none());

        // Non-RGB device spaces (CMYK, GRAY, Lab).
        for device in [b"CMYK", b"GRAY", b"Lab "] {
            let mut profile = lut16_xyz_a2b_profile();
            profile[16..20].copy_from_slice(device);
            assert!(DisplayTransform::for_profile(&profile).is_none());
        }
    }

    // -----------------------------------------------------------------------
    // The hostile-profile harness
    // -----------------------------------------------------------------------
    //
    // Everything above checks well-formed profiles against reference values;
    // nothing has tried to break one. The tests below do exactly that, from
    // three directions: truncation at every boundary a reader could trip on,
    // a tag directory that lies about what it points at, and bit-level
    // mutations of the same fixtures the cross-checks build. The contract
    // every case is held to is one sentence: `for_profile` either refuses —
    // quickly, without panicking — or returns a transform whose cube, and
    // whose answer for out-of-range and NaN inputs, is finite and in [0, 1].
    // A profile that cannot be read is refused; a profile that can be read
    // cannot poison the cube.

    mod fuzz_tests {
        use super::*;
        use std::panic::catch_unwind;
        use std::time::{Duration, Instant};

        /// The wall-clock ceiling for one profile, debug build. The honest
        /// worst case is the 33³ inverted bake at roughly 75 ms and the
        /// legal hostile worst is a chain of four CLUTs whose outputs no
        /// incoming PCS value reaches, at roughly 3.5 s — every vertex of
        /// that chain takes the bounded out-of-gamut fallback, and a fifth
        /// CLUT is a refusal rather than a longer bake. A profile past this
        /// ceiling is a wedge, whatever its bytes said.
        const CASE_BUDGET: Duration = Duration::from_millis(5_000);

        /// A deterministic xorshift64* sequence: no dependency to pull, and
        /// a failure replays from its seed.
        struct Rng(u64);

        impl Rng {
            fn new(seed: u64) -> Self {
                // Zero would repeat forever; any other seed does not.
                Self(seed.max(1))
            }

            fn next_u64(&mut self) -> u64 {
                let mut x = self.0;
                x ^= x >> 12;
                x ^= x << 25;
                x ^= x >> 27;
                self.0 = x;
                x.wrapping_mul(0x2545_F491_4F6C_DD1D)
            }

            fn byte(&mut self) -> u8 {
                self.next_u64() as u8
            }

            /// Uniform enough for flipping bytes: modulo bias is not a
            /// coverage problem at these sizes.
            fn below(&mut self, bound: usize) -> usize {
                (self.next_u64() % bound as u64) as usize
            }
        }

        /// The message inside a caught panic payload, for the failure line.
        fn panic_text(panic: &(dyn std::any::Any + Send)) -> String {
            if let Some(text) = panic.downcast_ref::<&str>() {
                (*text).to_owned()
            } else if let Some(text) = panic.downcast_ref::<String>() {
                text.clone()
            } else {
                "an unknown panic payload".to_owned()
            }
        }

        /// The contract on what `for_profile` hands back: the cube it baked
        /// and its answer for out-of-range and NaN inputs, all finite and
        /// in [0, 1]. Panics — failing the test — otherwise.
        fn assert_in_contract(
            parsed: Result<Option<DisplayTransform>, Box<dyn std::any::Any + Send>>,
        ) -> Option<DisplayTransform> {
            let transform = match parsed {
                Ok(transform) => transform,
                Err(panic) => panic!("for_profile panicked: {}", panic_text(&*panic)),
            };
            let transform = transform?;
            // The bake is the map over every vertex of the cube, so the
            // cube's entries carry the first half of the contract.
            for entry in transform.lut() {
                for channel in entry {
                    let value = f64::from(*channel);
                    assert!(
                        value.is_finite() && (0.0..=1.0).contains(&value),
                        "cube entry {entry:?} is out of contract"
                    );
                }
            }
            // The second half: hostile inputs. Out-of-range, infinite and
            // NaN channels are what a half-written upload or a racing client
            // can hand `map`; the answer has to be a colour either way.
            for input in [
                [0.5, 0.5, 0.5],
                [-0.25, 1.75, 0.5],
                [f64::NEG_INFINITY, f64::INFINITY, 0.0],
                [f64::NAN, 0.5, 0.5],
                [f64::NAN; 3],
            ] {
                let out = transform.map(input);
                for channel in out {
                    assert!(
                        channel.is_finite() && (0.0..=1.0).contains(&channel),
                        "map({input:?}) = {out:?} is out of contract"
                    );
                }
            }
            Some(transform)
        }

        /// One hostile profile against the whole contract, budget included.
        fn assert_contract(bytes: &[u8]) -> Option<DisplayTransform> {
            let started = Instant::now();
            let parsed = catch_unwind(|| DisplayTransform::for_profile(bytes));
            let elapsed = started.elapsed();
            assert!(
                elapsed < CASE_BUDGET,
                "one hostile profile took {elapsed:?}, past the {CASE_BUDGET:?} budget"
            );
            assert_in_contract(parsed)
        }

        /// A matrix-shaper profile with the given curve, the smallest honest
        /// matrix fixture: it parses, and mutating it exercises the path a
        /// display profile actually arrives by.
        fn matrix_profile(curve: &[u8]) -> Vec<u8> {
            let column =
                |index: usize| [SRGB_D50[0][index], SRGB_D50[1][index], SRGB_D50[2][index]];
            profile_with_tags(&[
                (b"rXYZ", xyz_tag(column(0))),
                (b"gXYZ", xyz_tag(column(1))),
                (b"bXYZ", xyz_tag(column(2))),
                (b"rTRC", curve.to_vec()),
                (b"gTRC", curve.to_vec()),
                (b"bTRC", curve.to_vec()),
            ])
        }

        /// One small, honest profile of every flavour the reader takes: a
        /// matrix-shaper, the 8-bit and 16-bit tables and the `mBA `/MPE
        /// elements, each in both the forward (`B2A`) and the inverted
        /// (`A2B`) direction. Grid-2 CLUTs and two-entry tables keep one
        /// bake down to a few milliseconds, so the sweeps below can afford
        /// thousands of cases.
        fn honest_small_profiles() -> Vec<(&'static str, Vec<u8>)> {
            let two = [identity_table(2), identity_table(2), identity_table(2)];
            let clut = |grid: usize| clut_nodes(grid, |xyz| xyz);
            let lut16 = |grid: usize| lut16_tag(&IDENTITY, &two, grid, &clut(grid), &two);
            // `mft1` carries no entry counts: its tables are 256 entries
            // whether the writer meant them or not.
            let lut8 = |grid: usize| {
                lut8_tag(
                    &IDENTITY,
                    &identity_tables(),
                    grid,
                    &clut(grid),
                    &identity_tables(),
                )
            };
            let b2a = |sig: &[u8; 4], tag: Vec<u8>| profile_with_tags(&[(sig, tag)]);
            let a2b =
                |sig: &[u8; 4], tag: Vec<u8>| profile_with_pcs_and_tags(b"XYZ ", &[(sig, tag)]);
            let lab_a2b =
                |sig: &[u8; 4], tag: Vec<u8>| profile_with_pcs_and_tags(b"Lab ", &[(sig, tag)]);
            vec![
                ("matrix-shaper", matrix_profile(&curv_gamma(2.2))),
                ("mft1 B2A", b2a(b"B2A1", lut8(2))),
                ("mft2 B2A", b2a(b"B2A1", lut16(2))),
                (
                    "mBA B2A",
                    profile_with_pcs_and_tags(
                        b"Lab ",
                        &[(
                            b"B2A1",
                            mba_tag(
                                Some(identity_curve_set()),
                                None,
                                None,
                                Some(mba_clut_bytes([2, 2, 2], &clut(2))),
                                Some(gamma_curve_set(1.0 / 2.2)),
                            ),
                        )],
                    ),
                ),
                (
                    "mpet B2D",
                    profile_with_pcs_and_tags(
                        b"Lab ",
                        &[(
                            b"B2D1",
                            mpe_tag(&[
                                cvst_formula_set(0, &[1.0, 1.0, 0.0, 0.0]),
                                clut_element([2, 2, 2], &clut(2)),
                                cvst_formula_set(0, &[1.0 / 2.2, 1.0, 0.0, 0.0]),
                            ]),
                        )],
                    ),
                ),
                ("mft1 A2B", a2b(b"A2B1", lut8(2))),
                ("mft2 A2B", a2b(b"A2B1", lut16(2))),
                (
                    "mAB A2B",
                    lab_a2b(
                        b"A2B1",
                        mab_tag(
                            Some(identity_curve_set()),
                            None,
                            None,
                            Some(mba_clut_bytes([2, 2, 2], &clut(2))),
                            Some(identity_curve_set()),
                        ),
                    ),
                ),
                (
                    "mpet D2B",
                    profile_with_pcs_and_tags(
                        b"Lab ",
                        &[(
                            b"D2B1",
                            mpe_tag(&[
                                cvst_formula_set(0, &[1.0, 1.0, 0.0, 0.0]),
                                // The normalize matrix raw Lab meets first,
                                // the way a real D2B carries it — without it
                                // the CLUT is indexed at fifty times its
                                // range and every vertex takes the
                                // out-of-gamut fallback.
                                matf_element(
                                    &[
                                        [1.0 / 100.0, 0.0, 0.0],
                                        [0.0, 1.0 / 255.0, 0.0],
                                        [0.0, 0.0, 1.0 / 255.0],
                                    ],
                                    &[0.0, 128.0 / 255.0, 128.0 / 255.0],
                                ),
                                clut_element([2, 2, 2], &clut(2)),
                                cvst_formula_set(0, &[1.0, 1.0, 0.0, 0.0]),
                            ]),
                        )],
                    ),
                ),
            ]
        }

        /// Rewrite a directory entry of `signature` in a profile built by
        /// [`profile_with_tags`] — the `occurrence`th one, counting from
        /// zero — so its offset and size become the given pair, whatever
        /// the pair says about the file.
        fn point_tag_at(
            profile: &mut [u8],
            signature: &[u8; 4],
            occurrence: usize,
            offset: u32,
            size: u32,
        ) {
            let count = be_u32(&profile[128..132]) as usize;
            let mut seen = 0usize;
            for at in (132..132 + count * 12).step_by(12) {
                if &profile[at..at + 4] == signature {
                    if seen == occurrence {
                        profile[at + 4..at + 8].copy_from_slice(&offset.to_be_bytes());
                        profile[at + 8..at + 12].copy_from_slice(&size.to_be_bytes());
                        return;
                    }
                    seen += 1;
                }
            }
            panic!("no {occurrence}th {signature:?} in the directory to move");
        }

        /// Where a tag's data starts, for surgery that keeps a slice honest.
        fn find_tag_offset(profile: &[u8], signature: &[u8; 4]) -> u32 {
            let count = be_u32(&profile[128..132]) as usize;
            for at in (132..132 + count * 12).step_by(12) {
                if &profile[at..at + 4] == signature {
                    return u32::from_be_bytes([
                        profile[at + 4],
                        profile[at + 5],
                        profile[at + 6],
                        profile[at + 7],
                    ]);
                }
            }
            panic!("no {signature:?} in the directory");
        }

        /// Whether the directory carries `signature` at all.
        fn has_tag(profile: &[u8], signature: &[u8; 4]) -> bool {
            let count = be_u32(&profile[128..132]) as usize;
            (132..132 + count * 12)
                .step_by(12)
                .any(|at| &profile[at..at + 4] == signature)
        }

        /// Truncation at every boundary a reader could trip on: the whole
        /// header and directory byte by byte, and — on the real fixtures,
        /// whose tags run to the end of the file — the middle of the CLUT
        /// nodes, where a short read is invisible unless the reader counts.
        #[test]
        fn truncated_profiles_are_refused_or_stay_in_contract() {
            let mut cuts = 0usize;
            for (name, bytes) in honest_small_profiles() {
                for len in 0..bytes.len() {
                    assert_contract(&bytes[..len]);
                    cuts += 1;
                }
                // The untruncated profile is the control: it must parse.
                assert!(
                    assert_contract(&bytes).is_some(),
                    "the honest {name} fixture must parse whole"
                );
            }
            // The cross-check fixtures are an order of magnitude bigger;
            // walk the last thirty kilobytes of each — the CLUT's nodes —
            // a node row at a time, where a hostile truncation hides: a
            // slice that no longer fits the file must be a refusal, not a
            // read past the end.
            for bytes in [
                lut16_lab_b2a_profile(),
                mba_lab_b2a_profile(),
                lut16_lab_a2b_profile(),
                mab_lab_a2b_profile(),
                mpe_lab_d2b_profile(),
            ] {
                let one_entry = bytes.len() - 132 - 12; // one tag, one entry
                let clut_start = bytes.len() - 17 * 17 * 17 * 3 * 2;
                for at in (clut_start..bytes.len()).step_by(17 * 3 * 2) {
                    assert_contract(&bytes[..at]);
                    assert_contract(&bytes[..at + 1]);
                    cuts += 2;
                }
                assert!(
                    assert_contract(&bytes[..one_entry]).is_none(),
                    "a directory without its tag is not a transform"
                );
            }
            println!("truncation: {cuts} cuts");
        }

        /// A tag directory that lies: offsets past the end of the file, an
        /// offset and size whose sum overflows, a zero size, tags that
        /// overlap, duplicate entries, and a count the directory cannot
        /// hold. A lie is either named and refused, or — when the slice it
        /// describes happens to be honest — read like any other profile;
        /// what no lie may do is half-read a transform.
        #[test]
        fn a_lying_tag_directory_is_refused_or_stays_in_contract() {
            let two = [identity_table(2), identity_table(2), identity_table(2)];
            let clut = clut_nodes(2, |xyz| xyz);
            let mut lies = 0usize;
            for (name, bytes) in honest_small_profiles() {
                // Offsets and sizes that point outside the file, at the
                // header, into the directory, or nowhere at all.
                let len = bytes.len() as u32;
                for (offset, size) in [
                    (u32::MAX, 1),
                    (0xFFFF_FFF0, 0x20),
                    (len - 5, 10),
                    (len.saturating_sub(4), 4),
                    (132, 20),
                    (0, 132),
                    (len / 2, len),
                    (u32::MAX, u32::MAX),
                    (16, 4),
                ] {
                    for signature in [b"rXYZ", b"rTRC", b"B2A1", b"A2B1", b"D2B1"] {
                        let mut lying = bytes.clone();
                        if has_tag(&lying, signature) {
                            point_tag_at(&mut lying, signature, 0, offset, size);
                            assert_contract(&lying);
                            lies += 1;
                        }
                    }
                }
                // A count the directory cannot hold is a corrupt profile,
                // not a large one; a count of zero is a profile without
                // tags, which is a refusal by another rule.
                let honest_count = be_u32(&bytes[128..132]);
                for count in [0u32, 1, 0xFFFF, 0x7FFF_FFFF, u32::MAX] {
                    let mut lying = bytes.clone();
                    lying[128..132].copy_from_slice(&count.to_be_bytes());
                    let accepted = assert_contract(&lying);
                    if count != honest_count {
                        assert!(
                            accepted.is_none(),
                            "{name} with a lying count of {count} must be refused"
                        );
                    }
                    lies += 1;
                }
                // A zero size is not a tag.
                for signature in [b"rXYZ", b"B2A1", b"A2B1"] {
                    let mut lying = bytes.clone();
                    if has_tag(&lying, signature) {
                        let offset = find_tag_offset(&lying, signature);
                        point_tag_at(&mut lying, signature, 0, offset, 0);
                        assert!(
                            assert_contract(&lying).is_none(),
                            "{name}: a zero-size tag must be refused"
                        );
                        lies += 1;
                    }
                }
            }
            // Duplicates: the first entry wins. A valid tag first and a
            // lying one second still reads; a lying one first is a refusal
            // that must not fall back to the second.
            let honest = lut16_tag(&IDENTITY, &two, 2, &clut, &two);
            let honest_two =
                profile_with_tags(&[(b"B2A1", honest.clone()), (b"B2A1", honest.clone())]);
            let offset = find_tag_offset(&honest_two, b"B2A1");
            let with_two = |first: u32, second: u32| {
                let mut profile = honest_two.clone();
                point_tag_at(&mut profile, b"B2A1", 0, first, honest.len() as u32);
                point_tag_at(&mut profile, b"B2A1", 1, second, honest.len() as u32);
                profile
            };
            assert!(
                assert_contract(&with_two(offset, u32::MAX)).is_some(),
                "a valid tag before a lying duplicate still reads"
            );
            assert!(
                assert_contract(&with_two(u32::MAX, offset)).is_none(),
                "a lying first duplicate must not fall back to the second"
            );
            // Overlapping tags: two signatures describing the same slice.
            let mut overlapping =
                profile_with_tags(&[(b"B2A1", honest.clone()), (b"A2B1", honest.clone())]);
            point_tag_at(&mut overlapping, b"A2B1", 0, offset, honest.len() as u32);
            assert_contract(&overlapping);
            lies += 4;
            println!("lying directory: {lies} cases");
        }

        /// Consistent-looking headers with absurd payloads: CLUT grids too
        /// big to allocate, entry and element counts that disagree with the
        /// bytes behind them, curves at the one-entry and maximum sizes,
        /// and matrices a hair from singular. Each is a named refusal, or —
        /// when the payload is honest, just strange — an in-contract
        /// transform; none may panic or bake its way out of [0, 1].
        #[test]
        fn absurd_payloads_are_refused_or_stay_in_contract() {
            let two = [identity_table(2), identity_table(2), identity_table(2)];
            let clut = |grid: usize| clut_nodes(grid, |xyz| xyz);
            let mut cases = 0usize;

            // CLUT grids: a byte wider than the entry cap, the maximum a
            // grid byte can say, the impossible single point, and a grid
            // whose nodes are not there.
            for grid in [2u8, 88, 89, 255] {
                let tag = lut16_tag(&IDENTITY, &two, usize::from(grid), &clut(2), &two);
                let accepted = assert_contract(&profile_with_tags(&[(b"B2A1", tag)]));
                if grid > 88 {
                    assert!(accepted.is_none(), "a grid of {grid} must be refused");
                }
                cases += 1;
            }
            // The largest grid the entry cap takes with its payload really
            // there: 88³ nodes is four megabytes of honest profile, and it
            // is a colour like any other.
            let full = lut16_tag(&IDENTITY, &two, 88, &clut(88), &two);
            assert!(
                assert_contract(&profile_with_tags(&[(b"B2A1", full)])).is_some(),
                "a complete 88^3 grid is the cap, not past it"
            );
            cases += 1;
            let mut grid_one = lut16_tag(&IDENTITY, &two, 1, &[], &two);
            grid_one[10] = 1;
            assert!(
                assert_contract(&profile_with_tags(&[(b"B2A1", grid_one)])).is_none(),
                "a grid of one point is not a CLUT"
            );
            cases += 1;
            // A grid of two with no nodes behind it is a refusal; a grid of
            // zero is the honest way to say "no CLUT", and the curves and
            // matrix it leaves are a transform like any other.
            let no_nodes = lut16_tag(&IDENTITY, &two, 2, &[], &two);
            assert!(
                assert_contract(&profile_with_tags(&[(b"B2A1", no_nodes.clone())])).is_none(),
                "a grid of two with no nodes must be refused"
            );
            let mut no_clut = no_nodes;
            no_clut[10] = 0;
            assert!(
                assert_contract(&profile_with_tags(&[(b"B2A1", no_clut)])).is_some(),
                "a table profile without a CLUT is still a transform"
            );
            cases += 2;

            // The same grids through the `mBA ` CLUT element, where the
            // precision byte is one more thing to lie about.
            for grid in [89u8, 255] {
                for precision in [0u8, 1, 2, 3] {
                    let mut bytes = mba_clut_bytes([grid, grid, grid], &clut(2));
                    bytes[16] = precision;
                    bytes.truncate(20);
                    let tag = mba_tag(None, None, None, Some(bytes), None);
                    assert!(
                        assert_contract(&profile_with_tags(&[(b"B2A1", tag)])).is_none(),
                        "grid {grid}, precision {precision} must be refused"
                    );
                    cases += 1;
                }
            }

            // Entry counts: the one-entry table (a corrupt table, not a
            // gamma — that spelling belongs to `curv`), the zero table
            // (absent, by extension), the maximum `mft2` allows, one past
            // it, and the maximum with no payload behind it.
            let one = [identity_table(1), identity_table(1), identity_table(1)];
            let big = [
                identity_table(0x7FFF),
                identity_table(0x7FFF),
                identity_table(0x7FFF),
            ];
            for (input, output) in [
                (&one[..], &two[..]),
                (&two[..], &one[..]),
                (&big[..], &two[..]),
                (&two[..], &big[..]),
            ] {
                let input: &[Vec<f64>; 3] = (*input).try_into().unwrap();
                let output: &[Vec<f64>; 3] = (*output).try_into().unwrap();
                let mut tag = lut16_tag(&IDENTITY, input, 2, &clut(2), output);
                let accepted = assert_contract(&profile_with_tags(&[(b"B2A1", tag.clone())]));
                if input.len() == 1 || output.len() == 1 {
                    assert!(accepted.is_none(), "one-entry tables must be refused");
                }
                cases += 1;
                // The maximum entry count with the payload missing behind
                // it — a count only the big variants can afford to lose.
                if input[0].len() == 0x7FFF {
                    tag.truncate(tag.len() - 0x7FFF * 2);
                    assert!(
                        assert_contract(&profile_with_tags(&[(b"B2A1", tag)])).is_none(),
                        "a 0x7FFF-entry table with no payload must be refused"
                    );
                    cases += 1;
                }
            }
            let over = [
                identity_table(0x8000),
                identity_table(0x8000),
                identity_table(0x8000),
            ];
            let over: &[Vec<f64>; 3] = &over;
            let tag = lut16_tag(&IDENTITY, over, 2, &clut(2), &two);
            assert!(
                assert_contract(&profile_with_tags(&[(b"B2A1", tag)])).is_none(),
                "an entry count past 0x7FFF must be refused"
            );
            cases += 1;

            // Curves at the extremes: the identity (a zero-count `curv`),
            // a one-entry gamma of zero, a negative and a huge gamma, a
            // `curv` count past anyone's payload, and every `para` kind
            // with parameters from the ends of the fixed-point range.
            // Each rides both the LUT path (as `mBA ` A curves) and the
            // matrix path (as TRCs), the two readers a curve can meet.
            let curv = |count: u32, entries: &[u16]| {
                let mut data = b"curv\0\0\0\0".to_vec();
                data.extend_from_slice(&count.to_be_bytes());
                for entry in entries {
                    data.extend_from_slice(&entry.to_be_bytes());
                }
                data
            };
            let primaries = |curve: &[u8]| {
                profile_with_tags(&[
                    (b"rXYZ", xyz_tag([0.436, 0.222, 0.014])),
                    (b"gXYZ", xyz_tag([0.385, 0.717, 0.097])),
                    (b"bXYZ", xyz_tag([0.143, 0.061, 0.714])),
                    (b"rTRC", curve.to_vec()),
                    (b"gTRC", curve.to_vec()),
                    (b"bTRC", curve.to_vec()),
                ])
            };
            for data in [
                curv(0, &[]),
                curv(1, &[0]),
                curv(1, &[0x8000]),
                curv(1, &[0xFFFF]),
                curv(0xFFFF_FFFF, &[0, 0]),
                curv(0xFFFF, &[]),
                curv(2, &[0, 0xFFFF]),
                curv(3, &[0, 20_000, 10_000]),
            ] {
                let set = curves_blob(&[data.clone(), data.clone(), data.clone()]);
                let tag = mba_tag(
                    Some(identity_curve_set()),
                    None,
                    None,
                    Some(mba_clut_bytes([2, 2, 2], &clut(2))),
                    Some(set),
                );
                assert_contract(&profile_with_pcs_and_tags(b"Lab ", &[(b"B2A1", tag)]));
                cases += 1;
                assert_contract(&primaries(&data));
                cases += 1;
            }
            for kind in 0u16..5 {
                for value in [i32::MIN, -65_536, 0, 65_536, i32::MAX] {
                    let mut data = b"para\0\0\0\0".to_vec();
                    data.extend_from_slice(&kind.to_be_bytes());
                    data.extend_from_slice(&0u16.to_be_bytes());
                    for _ in 0..7 {
                        data.extend_from_slice(&value.to_be_bytes());
                    }
                    assert_contract(&primaries(&data));
                    cases += 1;
                }
            }

            // Matrix stages a hair from singular, exactly singular, and
            // scaled to the ends of the fixed-point range — through the
            // fixed-point `mAB ` element an inverted pipeline inverts, the
            // `mft` inline matrix the forward one reads, and the float
            // `matf`, where a hair from singular survives quantization.
            let near_fixed = [
                [1.0, 0.0, 0.0],
                [1.0 + 4.0 / 65_536.0, 0.0, 0.0],
                [0.0, 0.0, 1.0],
            ];
            let near_float = [[1.0, 0.0, 0.0], [1.0 + 1e-7, 0.0, 0.0], [0.0, 0.0, 1.0]];
            for matrix in [
                near_fixed,
                [[1.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]],
                [[32_768.0, -32_768.0, 32_768.0]; 3],
                [[1e-4, 0.0, 0.0], [0.0, 1e-4, 0.0], [0.0, 0.0, 1e-4]],
            ] {
                let tag = mab_tag(
                    Some(identity_curve_set()),
                    Some(matrix_element_bytes(&matrix, &[0.0; 3])),
                    None,
                    None,
                    None,
                );
                assert_contract(&profile_with_pcs_and_tags(b"XYZ ", &[(b"A2B1", tag)]));
                cases += 1;
                let inline = lut16_tag(&matrix, &two, 2, &clut(2), &two);
                assert_contract(&profile_with_tags(&[(b"B2A1", inline)]));
                cases += 1;
            }
            let near = matf_element(&near_float, &[0.0; 3]);
            assert_contract(&profile_with_pcs_and_tags(
                b"Lab ",
                &[(b"D2B1", mpe_tag(&[near]))],
            ));
            cases += 1;

            // `mpet`: element counts the position table cannot hold,
            // offsets into the position table itself, elements with no
            // size, unknown signatures, and a count of zero.
            let identity_element = cvst_formula_set(0, &[1.0, 1.0, 0.0, 0.0]);
            let clut_element = clut_element([2, 2, 2], &clut(2));
            let honest = mpe_tag(&[identity_element.clone(), clut_element.clone()]);
            let mut lying = honest.clone();
            lying[12..16].copy_from_slice(&0xFFFF_FFFFu32.to_be_bytes());
            assert!(
                assert_contract(&profile_with_tags(&[(b"B2D1", lying.clone())])).is_none(),
                "an element count past the payload must be refused"
            );
            cases += 1;
            for offset in [0u32, 4, 8, 12, 15] {
                let mut into_table = honest.clone();
                // The first element's offset, aimed back at the table.
                into_table[16..20].copy_from_slice(&offset.to_be_bytes());
                assert!(
                    assert_contract(&profile_with_tags(&[(b"B2D1", into_table.clone())])).is_none(),
                    "an element offset inside the position table must be refused"
                );
                cases += 1;
            }
            let mut zero_size = honest.clone();
            zero_size[16 + 4..16 + 8].copy_from_slice(&0u32.to_be_bytes());
            assert!(
                assert_contract(&profile_with_tags(&[(b"B2D1", zero_size)])).is_none(),
                "a zero-size element must be refused"
            );
            cases += 1;
            let mut unknown_element = identity_element.clone();
            unknown_element[0..4].copy_from_slice(b"xxxx");
            let unknown = mpe_tag(&[unknown_element, clut_element.clone()]);
            assert!(
                assert_contract(&profile_with_tags(&[(b"B2D1", unknown)])).is_none(),
                "an unknown element signature must be refused"
            );
            let mut zero = honest.clone();
            zero[12..16].copy_from_slice(&0u32.to_be_bytes());
            assert!(
                assert_contract(&profile_with_tags(&[(b"B2D1", zero)])).is_none(),
                "an empty position table is not a pipeline"
            );
            cases += 3;

            // A pipeline of many curve stages is legal by the letter and
            // its cost is a constant, not its size; a chain of CLUTs is
            // where an inverted bake multiplies, so five is a refusal.
            for count in [8usize, 64, 128] {
                let elements: Vec<Vec<u8>> = (0..count).map(|_| identity_element.clone()).collect();
                let tag = mpe_tag(&elements);
                let accepted = assert_contract(&profile_with_tags(&[(b"B2D1", tag)]));
                assert!(
                    accepted.is_some(),
                    "a pipeline of {count} small honest curve stages is still a profile"
                );
                cases += 1;
            }
            for (count, cluts) in [(8usize, 4usize), (64, 32), (256, 128)] {
                let elements: Vec<Vec<u8>> = (0..count)
                    .map(|i| {
                        if i % 2 == 0 {
                            identity_element.clone()
                        } else {
                            clut_element.clone()
                        }
                    })
                    .collect();
                let tag = mpe_tag(&elements);
                let accepted = assert_contract(&profile_with_tags(&[(b"B2D1", tag)]));
                if cluts > PIPELINE_CLUT_CAP {
                    assert!(
                        accepted.is_none(),
                        "a pipeline of {count} stages carries more CLUTs than the cap allows"
                    );
                } else {
                    assert!(
                        accepted.is_some(),
                        "a pipeline of {cluts} CLUTs is at the cap, not past it"
                    );
                }
                cases += 1;
            }

            // A segmented curve at the segment cap, and one whose sampled
            // pieces overflow the sample cap: a small file either way, and
            // the reader must not trust either count.
            let mut many_breaks = b"curf\0\0\0\0".to_vec();
            many_breaks.extend_from_slice(&(SEGMENT_CAP as u16).to_be_bytes());
            many_breaks.extend_from_slice(&0u16.to_be_bytes());
            for i in 0..SEGMENT_CAP - 1 {
                many_breaks.extend_from_slice(&(i as f32).to_be_bytes());
            }
            for _ in 0..SEGMENT_CAP {
                many_breaks.extend_from_slice(b"parf\0\0\0\0");
                many_breaks.extend_from_slice(&0u16.to_be_bytes());
                many_breaks.extend_from_slice(&0u16.to_be_bytes());
                for param in [1.0f32, 1.0, 0.0, 0.0] {
                    many_breaks.extend_from_slice(&param.to_be_bytes());
                }
            }
            assert_contract(&profile_with_tags(&[(
                b"B2D1",
                mpe_tag(&[
                    cvst_element(&[many_breaks.clone(), many_breaks.clone(), many_breaks]),
                    clut_element.clone(),
                ]),
            )]));
            cases += 1;
            let mut huge_samples = b"curf\0\0\0\0".to_vec();
            huge_samples.extend_from_slice(&1u16.to_be_bytes());
            huge_samples.extend_from_slice(&0u16.to_be_bytes());
            huge_samples.extend_from_slice(b"samf\0\0\0\0");
            huge_samples.extend_from_slice(&u32::MAX.to_be_bytes());
            assert!(
                assert_contract(&profile_with_tags(&[(
                    b"B2D1",
                    mpe_tag(&[cvst_element(&[
                        huge_samples.clone(),
                        huge_samples.clone(),
                        huge_samples,
                    ])]),
                )]))
                .is_none(),
                "a sampled piece with no samples behind it must be refused"
            );
            cases += 1;

            println!("absurd payloads: {cases} cases");
        }

        /// One mutation of `bytes`: a bit flip, a set or cleared byte, a
        /// byte swap, a duplicated run, a reversed run, a zeroed run, or an
        /// endian swap at a word boundary — the ways a profile corrupts in
        /// the wild and the ways a hostile one is written.
        fn mutate(rng: &mut Rng, bytes: &[u8]) -> Vec<u8> {
            let mut out = bytes.to_vec();
            match rng.below(7) {
                0 => {
                    let at = rng.below(out.len());
                    out[at] ^= 1 << rng.below(8);
                }
                1 => {
                    let at = rng.below(out.len());
                    out[at] = rng.byte();
                }
                2 => {
                    let at = rng.below(out.len());
                    out[at] = if rng.below(2) == 0 { 0x00 } else { 0xFF };
                }
                3 => {
                    let a = rng.below(out.len());
                    let b = rng.below(out.len());
                    out.swap(a, b);
                }
                4 => {
                    // Duplicate a run, growing the file the way a writer
                    // that repeats a node does.
                    let start = rng.below(out.len());
                    let len = (rng.below(16) + 1).min(out.len() - start);
                    let run = out[start..start + len].to_vec();
                    let at = rng.below(out.len());
                    out.splice(at..at, run);
                }
                5 => {
                    let start = rng.below(out.len());
                    let len = (rng.below(16) + 1).min(out.len() - start);
                    out[start..start + len].reverse();
                }
                _ => {
                    let start = rng.below(out.len() - 3);
                    let len = (rng.below(32) + 1).min(out.len() - start);
                    for byte in &mut out[start..start + len] {
                        *byte = 0;
                    }
                }
            }
            out
        }

        /// Mutations of the honest fixtures: whatever a flipped bit or a
        /// duplicated node does to a profile, the reader either refuses it
        /// or bakes a cube in contract. Two hundred and fifty mutants per
        /// fixture, from a fixed seed: a failure replays.
        #[test]
        fn mutated_fixtures_are_refused_or_stay_in_contract() {
            let mut rng = Rng::new(0x1CC_F17F_2026);
            let mut cases = 0usize;
            let mut accepted = 0usize;
            for (name, bytes) in honest_small_profiles() {
                // The inverted flavours solve at bake time, so a mutant of
                // one costs an order of magnitude more than a mutant of a
                // forward flavour; the counts keep the sweep seconds-scale
                // while every flavour still meets hundreds of mutations.
                let mutants = if name.contains("A2B") || name.contains("D2B") {
                    15
                } else {
                    40
                };
                for index in 0..mutants {
                    let mutant = mutate(&mut rng, &bytes);
                    let started = Instant::now();
                    let parsed = catch_unwind(|| DisplayTransform::for_profile(&mutant));
                    let elapsed = started.elapsed();
                    assert!(
                        elapsed < CASE_BUDGET,
                        "{name} mutant #{index} baked for {elapsed:?}, past the budget"
                    );
                    if assert_in_contract(parsed).is_some() {
                        accepted += 1;
                    }
                    cases += 1;
                }
                // The unmutated control again: the mutations above must
                // have been measured against a profile that parses.
                assert!(
                    assert_contract(&bytes).is_some(),
                    "the honest {name} fixture must parse whole"
                );
            }
            println!("mutations: {cases} cases, {accepted} still parsed");
        }

        /// A CLUT degenerate in the middle, not the whole: one flat cell
        /// among valid ones, one folded cell among valid ones, and one
        /// flat cell in a real-sized 17³ grid where the solve has to work
        /// around it thousands of times over. The folded cell is a refusal
        /// — the pipeline must name it, not guess around it — and the flat
        /// one is underdetermined, so the answer just has to be in
        /// contract.
        #[test]
        fn a_clut_degenerate_in_the_middle_is_refused_or_bounded() {
            let two = [identity_table(2), identity_table(2), identity_table(2)];
            // Where one node of a grid lives in the f64 node array
            // `clut_nodes` builds: first channel slowest, three outputs.
            let node = |grid: usize, at: [usize; 3], channel: usize| {
                ((at[0] * grid + at[1]) * grid + at[2]) * 3 + channel
            };
            // One flat cell in a 9³ identity CLUT: every corner of the cell
            // at (3, 3, 3) says [0.4, 0.4, 0.4].
            let mut flat = clut_nodes(9, |xyz| xyz);
            for i in [3usize, 4] {
                for j in [3usize, 4] {
                    for k in [3usize, 4] {
                        for channel in 0..3 {
                            flat[node(9, [i, j, k], channel)] = 0.4;
                        }
                    }
                }
            }
            let tag = lut16_tag(&IDENTITY, &two, 9, &flat, &two);
            assert!(
                assert_contract(&profile_with_tags(&[(b"A2B1", tag)])).is_some(),
                "one flat cell among valid ones is invertible enough to take"
            );

            // One folded cell in the same grid: the whole x-face of the
            // cell is reversed, so the first axis runs backwards inside it
            // — a fold no honest profiler writes.
            let mut folded = clut_nodes(9, |xyz| xyz);
            for j in [3usize, 4] {
                for k in [3usize, 4] {
                    for channel in 0..3 {
                        folded.swap(node(9, [3, j, k], channel), node(9, [4, j, k], channel));
                    }
                }
            }
            let tag = lut16_tag(&IDENTITY, &two, 9, &folded, &two);
            assert!(
                assert_contract(&profile_with_tags(&[(b"A2B1", tag)])).is_none(),
                "a folded cell is a named refusal, not a guess"
            );

            // A flat slab at 17³, the size a real profiler ships, valued to
            // match the field around it — flat, not folded: a slab stuck in
            // at a value the field has already passed would fold the
            // boundary cells, and the orientation check refuses that the
            // same way it refused the folded cell above. The solve meets
            // this flat region thousands of times and has to stay bounded
            // every one of them.
            let mut flat17 = clut_nodes(17, |device| {
                [
                    device[0].powf(2.2),
                    device[1].powf(2.2),
                    device[2].powf(2.2),
                ]
            });
            let slab = (8.0 / 16.0f64).powf(2.2);
            for i in 0..17 * 3 {
                flat17[node(17, [8, 8, 8], 0) + i] = slab;
            }
            let tag = lut16_tag(&IDENTITY, &two, 17, &flat17, &two);
            assert!(
                assert_contract(&profile_with_pcs_and_tags(b"XYZ ", &[(b"A2B1", tag)])).is_some(),
                "a flat slab in an honest 17³ is still a profile"
            );
            // The same slab at a value the field has passed — 0.4 where
            // the field says 0.2 — folds at its boundary, which is a
            // refusal.
            let mut folded17 = flat17;
            for i in 0..17 * 3 {
                folded17[node(17, [8, 8, 8], 0) + i] = 0.4;
            }
            let tag = lut16_tag(&IDENTITY, &two, 17, &folded17, &two);
            assert!(
                assert_contract(&profile_with_pcs_and_tags(b"XYZ ", &[(b"A2B1", tag)])).is_none(),
                "a slab that folds the boundary cells is a named refusal"
            );

            // A CLUT whose outputs no incoming PCS value ever reaches: the
            // parse-time checks all pass — the table is finite, oriented
            // and spans every axis — and every vertex of the bake takes
            // the bounded out-of-gamut fallback instead. That fallback is
            // the most expensive thing a single stage can do (about 0.9 s
            // for the whole cube in debug), it is also what an honest
            // narrow-gamut display does with the colours it cannot show,
            // and the cap on CLUT stages is what keeps a hostile chain of
            // such tables from multiplying it.
            let unreachable = clut_element(
                [2, 2, 2],
                &clut_nodes(2, |xyz| [xyz[0] + 1.5, xyz[1] + 1.5, xyz[2] + 1.5]),
            );
            let identity = cvst_formula_set(0, &[1.0, 1.0, 0.0, 0.0]);
            let tag = mpe_tag(&[identity.clone(), unreachable, identity]);
            assert!(
                assert_contract(&profile_with_pcs_and_tags(b"Lab ", &[(b"D2B1", tag)])).is_some(),
                "a CLUT out of the PCS's reach is a profile, clamped"
            );
            println!("degenerate cells: 6 cases");
        }

        /// `map` is the renderer's entry point, and the cube is not the
        /// only thing that goes through it: anything can arrive, and NaN
        /// channels out are black pixels, not colours. Every flavour of
        /// profile must agree.
        #[test]
        fn nan_and_extreme_inputs_map_to_finite_values_for_every_flavour() {
            for (name, bytes) in honest_small_profiles() {
                let transform = assert_contract(&bytes).expect("the honest fixture parses");
                for input in [
                    [f64::NAN; 3],
                    [f64::NAN, 0.0, 1.0],
                    [0.0, f64::NAN, 0.5],
                    [f64::INFINITY; 3],
                    [f64::NEG_INFINITY, 0.5, f64::INFINITY],
                    [-7.0, 8.0, 0.5],
                ] {
                    let out = transform.map(input);
                    for channel in out {
                        assert!(
                            channel.is_finite() && (0.0..=1.0).contains(&channel),
                            "{name}: map({input:?}) = {out:?}"
                        );
                    }
                }
            }
        }

        /// believed: a 33³ forward (`B2A`) profile and a 33³ inverted
        /// (`A2B`) one, both the same displays the cross-check fixtures
        /// describe, then the hostile constants the harness states — the
        /// out-of-gamut fallback once and at the four-stage cap. Run with
        /// `cargo test --release -- --ignored --nocapture bake` and read
        /// the lines it prints; debug numbers come from the same test
        /// without `--release`.
        #[test]
        #[ignore]
        fn dump_bake_times() {
            let encode = gamma_table(256, 2.2);
            let output = [encode.clone(), encode.clone(), encode];
            let forward = lut16_tag(
                &IDENTITY,
                &identity_tables(),
                33,
                &clut_nodes(33, pcs_to_linear),
                &output,
            );
            let started = Instant::now();
            DisplayTransform::for_profile(&profile_with_tags(&[(b"B2A1", forward)]))
                .expect("the forward profile parses");
            println!("33^3 forward bake: {:?}", started.elapsed());

            let inverted = mab_tag(
                Some(identity_curve_set()),
                None,
                None,
                Some(mba_clut_bytes(
                    [33, 33, 33],
                    &clut_nodes(33, |linear| {
                        let xyz = pcs_from_linear(linear);
                        [
                            xyz[0] * XYZ_PCS_SCALE,
                            xyz[1] * XYZ_PCS_SCALE,
                            xyz[2] * XYZ_PCS_SCALE,
                        ]
                    }),
                )),
                Some(gamma_curve_set(2.2)),
            );
            let started = Instant::now();
            DisplayTransform::for_profile(&profile_with_pcs_and_tags(
                b"XYZ ",
                &[(b"A2B1", inverted)],
            ))
            .expect("the inverted profile parses");
            println!("33^3 inverted bake: {:?}", started.elapsed());

            // The hostile constants the harness states: the out-of-gamut
            // fallback a narrow-gamut display or a lying CLUT puts every
            // vertex on, once and at the four-stage cap.
            let unreachable = |shift: f64| {
                clut_element(
                    [2, 2, 2],
                    &clut_nodes(2, move |xyz| {
                        [xyz[0] + shift, xyz[1] + shift, xyz[2] + shift]
                    }),
                )
            };
            let identity = cvst_formula_set(0, &[1.0, 1.0, 0.0, 0.0]);
            let single = mpe_tag(&[identity.clone(), unreachable(1.5), identity.clone()]);
            let started = Instant::now();
            DisplayTransform::for_profile(&profile_with_pcs_and_tags(
                b"Lab ",
                &[(b"D2B1", single)],
            ))
            .expect("the unreachable-CLUT profile parses");
            println!(
                "one unreachable CLUT (fallback constant): {:?}",
                started.elapsed()
            );
            let chain = mpe_tag(&[
                identity.clone(),
                unreachable(1.5),
                identity.clone(),
                unreachable(1.5),
                identity.clone(),
                unreachable(1.5),
                identity.clone(),
                unreachable(1.5),
            ]);
            let started = Instant::now();
            DisplayTransform::for_profile(&profile_with_pcs_and_tags(b"Lab ", &[(b"D2B1", chain)]))
                .expect("the four-stage chain parses");
            println!(
                "four unreachable CLUTs (legal hostile worst): {:?}",
                started.elapsed()
            );
        }
    }

    /// The ΔE00 helper matches published reference pairs from Sharma, Wu and
    /// Dalal (2005).
    #[test]
    fn ciede2000_matches_the_sharma_reference_pairs() {
        let pairs = [
            ([50.0, 2.6772, -79.7751], [50.0, 0.0, -82.7485], 2.0425),
            ([50.0, 3.1571, -77.2803], [50.0, 0.0, -82.7485], 2.8615),
            ([50.0, 2.8361, -74.0200], [50.0, 0.0, -82.7485], 3.4412),
            ([50.0, -1.3802, -84.2814], [50.0, 0.0, -82.7485], 1.0000),
            ([50.0, 0.0, 0.0], [50.0, -1.0, 2.0], 2.3669),
            ([50.0, 2.5, 0.0], [73.0, 25.0, -18.0], 27.1492),
            ([50.0, 2.5, 0.0], [61.0, -5.0, 29.0], 22.8977),
            ([50.0, 2.5, 0.0], [50.0, 3.2972, 0.0], 1.0000),
            ([50.0, 2.5, 0.0], [50.0, 1.8634, 0.5757], 1.0000),
            ([50.0, 2.5, 0.0], [50.0, 2.5, 0.0], 0.0),
        ];
        for (lab1, lab2, wanted) in pairs {
            let got = ciede2000(lab1, lab2);
            assert!(
                (got - wanted).abs() < 1e-4,
                "ΔE00({lab1:?}, {lab2:?}) is {got}, reference says {wanted}"
            );
        }
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
