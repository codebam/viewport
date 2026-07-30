// SPDX-License-Identifier: MIT
//
// Transfer functions and primaries.
//
// This is the reason the renderer is Vulkan rather than GLES. Compositing an
// HDR surface next to an SDR one means every surface has to be decoded to
// light, moved into a shared set of primaries, and re-encoded for the output.
// Guessing any part of that produces an image that looks plausible and is
// wrong, which is worse than one that looks broken.
//
// All of it is here, in Rust, so it can be checked against known values
// without a GPU. The fragment shader is a translation of these functions and
// nothing more.

/// How encoded values relate to light.
///
/// The names are the ones `wp_color_management_v1` uses, because that is what
/// clients will be describing their buffers with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TransferFunction {
    /// The sRGB piecewise curve. What almost every SDR buffer is.
    #[default]
    Srgb,
    /// Already light-linear. What an intermediate buffer usually is.
    Linear,
    /// A pure power curve, used by some displays and by BT.1886 content.
    Gamma22,
    Gamma28,
    /// SMPTE ST 2084, the perceptual quantiser. Absolute: 1.0 means 10000
    /// cd/m², not "as bright as the display goes".
    Pq,
    /// Hybrid log-gamma. Relative, unlike PQ.
    Hlg,
}

/// The PQ peak, in cd/m². Unlike every other transfer function here, PQ
/// encodes absolute luminance, so this constant is part of its definition
/// rather than a display property.
pub const PQ_PEAK_LUMINANCE: f32 = 10_000.0;

impl TransferFunction {
    /// Encoded value to light, normalised so 1.0 is the reference white.
    ///
    /// For PQ that means dividing out the absolute peak: a compositor works in
    /// relative light and applies the display's luminance at the end, so
    /// leaving PQ's 10000 cd/m² in would make everything else 0.
    /// Whether this curve carries real luminance rather than a fraction of a
    /// reference white.
    ///
    /// PQ does: its 1.0 is 10,000 cd/m² whatever the display. Everything else
    /// here is relative, and the two cannot be mixed without saying what 1.0
    /// is worth on each side.
    pub fn is_absolute(self) -> bool {
        matches!(self, Self::Pq)
    }

    pub fn to_linear(self, value: f32) -> f32 {
        match self {
            Self::Linear => value,
            Self::Srgb => {
                // The piecewise curve, not the 2.2 power it is often mistaken
                // for. The linear segment near black is why they differ.
                if value <= 0.040_45 {
                    value / 12.92
                } else {
                    ((value + 0.055) / 1.055).powf(2.4)
                }
            }
            Self::Gamma22 => value.max(0.0).powf(2.2),
            Self::Gamma28 => value.max(0.0).powf(2.8),
            Self::Pq => {
                const M1: f32 = 0.159_301_76;
                const M2: f32 = 78.843_75;
                const C1: f32 = 0.835_937_5;
                const C2: f32 = 18.851_563;
                const C3: f32 = 18.687_5;

                let value = value.clamp(0.0, 1.0);
                let powed = value.powf(1.0 / M2);
                let numerator = (powed - C1).max(0.0);
                let denominator = C2 - C3 * powed;
                if denominator <= 0.0 {
                    return 0.0;
                }
                (numerator / denominator).powf(1.0 / M1)
            }
            Self::Hlg => {
                const A: f32 = 0.178_832_77;
                const B: f32 = 0.284_668_92;
                const C: f32 = 0.559_910_7;

                let value = value.clamp(0.0, 1.0);
                if value <= 0.5 {
                    (value * value) / 3.0
                } else {
                    (((value - C) / A).exp() + B) / 12.0
                }
            }
        }
    }

    /// Light back to an encoded value. The inverse of [`Self::to_linear`].
    pub fn from_linear(self, value: f32) -> f32 {
        match self {
            Self::Linear => value,
            Self::Srgb => {
                if value <= 0.003_130_8 {
                    value * 12.92
                } else {
                    1.055 * value.max(0.0).powf(1.0 / 2.4) - 0.055
                }
            }
            Self::Gamma22 => value.max(0.0).powf(1.0 / 2.2),
            Self::Gamma28 => value.max(0.0).powf(1.0 / 2.8),
            Self::Pq => {
                const M1: f32 = 0.159_301_76;
                const M2: f32 = 78.843_75;
                const C1: f32 = 0.835_937_5;
                const C2: f32 = 18.851_563;
                const C3: f32 = 18.687_5;

                let value = value.max(0.0);
                let powed = value.powf(M1);
                ((C1 + C2 * powed) / (1.0 + C3 * powed)).powf(M2)
            }
            Self::Hlg => {
                const A: f32 = 0.178_832_77;
                const B: f32 = 0.284_668_92;
                const C: f32 = 0.559_910_7;

                let value = value.max(0.0);
                if value <= 1.0 / 12.0 {
                    (3.0 * value).sqrt()
                } else {
                    A * (12.0 * value - B).ln() + C
                }
            }
        }
    }

    /// What the shader is told, since GLSL has no enums.
    pub fn as_code(self) -> u32 {
        match self {
            Self::Linear => 0,
            Self::Srgb => 1,
            Self::Gamma22 => 2,
            Self::Gamma28 => 3,
            Self::Pq => 4,
            Self::Hlg => 5,
        }
    }
}

/// The chromaticities of a colour space, as CIE xy pairs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Primaries {
    pub red: (f32, f32),
    pub green: (f32, f32),
    pub blue: (f32, f32),
    pub white: (f32, f32),
}

/// D65, the white point almost everything uses.
pub const D65: (f32, f32) = (0.3127, 0.3290);

impl Primaries {
    /// BT.709, which is also sRGB's. The SDR default.
    pub const SRGB: Self = Self {
        red: (0.640, 0.330),
        green: (0.300, 0.600),
        blue: (0.150, 0.060),
        white: D65,
    };

    /// BT.2020. The wide gamut HDR content is usually graded in.
    pub const BT2020: Self = Self {
        red: (0.708, 0.292),
        green: (0.170, 0.797),
        blue: (0.131, 0.046),
        white: D65,
    };

    /// Display P3 — DCI-P3 primaries with a D65 white point, which is what
    /// consumer displays actually use.
    pub const DISPLAY_P3: Self = Self {
        red: (0.680, 0.320),
        green: (0.265, 0.690),
        blue: (0.150, 0.060),
        white: D65,
    };

    pub const ADOBE_RGB: Self = Self {
        red: (0.640, 0.330),
        green: (0.210, 0.710),
        blue: (0.150, 0.060),
        white: D65,
    };

    /// The matrix taking linear RGB in this space to CIE XYZ.
    ///
    /// The standard construction: the primaries give the directions of the
    /// three columns, and the white point fixes their lengths — because the
    /// definition of a colour space's white is that equal RGB lands on it.
    pub fn to_xyz(&self) -> [[f32; 3]; 3] {
        let xyz = |(x, y): (f32, f32)| -> [f32; 3] {
            // Y is normalised to 1; the chromaticity only fixes the ratios.
            [x / y, 1.0, (1.0 - x - y) / y]
        };

        let r = xyz(self.red);
        let g = xyz(self.green);
        let b = xyz(self.blue);
        let w = xyz(self.white);

        // Solve [r g b] * s = w for the per-column scale factors.
        let m = [
            [r[0], g[0], b[0]],
            [r[1], g[1], b[1]],
            [r[2], g[2], b[2]],
        ];
        let s = match invert(&m) {
            Some(inverse) => multiply_vector(&inverse, w),
            // Degenerate primaries. Returning the identity keeps the renderer
            // running with the wrong gamut rather than failing a frame.
            None => [1.0, 1.0, 1.0],
        };

        [
            [r[0] * s[0], g[0] * s[1], b[0] * s[2]],
            [r[1] * s[0], g[1] * s[1], b[1] * s[2]],
            [r[2] * s[0], g[2] * s[1], b[2] * s[2]],
        ]
    }

    /// The matrix converting linear RGB in `self` to linear RGB in `to`.
    ///
    /// Both white points are assumed equal — everything here is D65 — so no
    /// chromatic adaptation is applied. Adding a non-D65 space means adding a
    /// Bradford transform, and doing it silently would be worse than not
    /// supporting it.
    pub fn convert_to(&self, to: &Primaries) -> [[f32; 3]; 3] {
        if self == to {
            return IDENTITY;
        }
        let Some(inverse) = invert(&to.to_xyz()) else {
            return IDENTITY;
        };
        multiply(&inverse, &self.to_xyz())
    }
}

/// The scale between two descriptions' linear values.
///
/// PQ is absolute: 1.0 is 10,000 cd/m², and every other transfer function
/// here is relative to a reference white. Converting between the two means
/// changing what 1.0 means, and treating them as if they already agreed is
/// what encoded an SDR desktop as 10,000 nits — every pixel driven to the
/// panel's limit, which is the washed-out white an HDR output showed.
pub fn luminance_scale(from: &Description, to: &Description) -> f32 {
    /// What PQ's 1.0 means, in cd/m².
    const PQ_PEAK: f32 = 10_000.0;

    match (from.transfer.is_absolute(), to.transfer.is_absolute()) {
        // Both relative, or both absolute: only the reference whites differ.
        (false, false) => from.reference_luminance / to.reference_luminance,
        (true, true) => 1.0,
        // Relative into absolute: SDR white is `reference_luminance` nits, and
        // PQ wants that as a fraction of 10,000.
        (false, true) => from.reference_luminance / PQ_PEAK,
        // Absolute into relative: the other way about.
        (true, false) => PQ_PEAK / to.reference_luminance,
    }
}

pub const IDENTITY: [[f32; 3]; 3] = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

/// Everything the renderer needs to know about one image's colour.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Description {
    pub transfer: TransferFunction,
    pub primaries: Primaries,
    /// The luminance, in cd/m², that the encoded value 1.0 represents.
    ///
    /// Only meaningful for relative transfer functions. PQ carries absolute
    /// luminance of its own, which is why it is divided out on decode.
    pub reference_luminance: f32,
}

impl Default for Description {
    fn default() -> Self {
        // Plain SDR sRGB: what a buffer is when the client says nothing.
        Self {
            transfer: TransferFunction::Srgb,
            primaries: Primaries::SRGB,
            reference_luminance: 203.0,
        }
    }
}

/// The description a surface has declared, or `None` for "assume sRGB".
///
/// It lives here rather than beside the protocol code because the renderer is
/// what has to read it. A buffer arrives through `ImportDmaWl`/`ImportMemWl`,
/// which are handed the surface's `SurfaceData` and nothing else; a type
/// defined in the compositor crate could not be looked up from there, and the
/// description would have been recorded and then never used — which is what
/// happened.
#[derive(Debug, Default)]
pub struct SurfaceColor(pub std::sync::Mutex<Option<Description>>);

/// What a surface's buffers contain, or the sRGB default.
///
/// The default is not a guess so much as the protocol's own answer: a client
/// that has not said anything is required to be treated as sRGB.
pub fn description_for(
    surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
) -> Description {
    smithay::wayland::compositor::with_states(surface, description_in)
}

/// The same, from a surface's state rather than the surface.
pub fn description_in(states: &smithay::wayland::compositor::SurfaceData) -> Description {
    states
        .data_map
        .get::<SurfaceColor>()
        .and_then(|color| color.0.lock().ok().and_then(|held| *held))
        .unwrap_or_default()
}

impl Description {
    /// Convert one encoded RGB triple into another description's encoding.
    ///
    /// The reference implementation the shader is checked against.
    pub fn convert(&self, to: &Description, rgb: [f32; 3]) -> [f32; 3] {
        let linear = [
            self.transfer.to_linear(rgb[0]),
            self.transfer.to_linear(rgb[1]),
            self.transfer.to_linear(rgb[2]),
        ];
        let matrix = self.primaries.convert_to(&to.primaries);
        let converted = multiply_vector(&matrix, linear);
        let scale = luminance_scale(self, to);
        [
            to.transfer.from_linear(converted[0] * scale),
            to.transfer.from_linear(converted[1] * scale),
            to.transfer.from_linear(converted[2] * scale),
        ]
    }
}

fn multiply(a: &[[f32; 3]; 3], b: &[[f32; 3]; 3]) -> [[f32; 3]; 3] {
    let mut out = [[0.0; 3]; 3];
    for (i, row) in out.iter_mut().enumerate() {
        for (j, cell) in row.iter_mut().enumerate() {
            *cell = (0..3).map(|k| a[i][k] * b[k][j]).sum();
        }
    }
    out
}

pub fn multiply_vector(m: &[[f32; 3]; 3], v: [f32; 3]) -> [f32; 3] {
    [
        m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
        m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
        m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
    ]
}

/// Invert a 3x3, or `None` if it is singular.
fn invert(m: &[[f32; 3]; 3]) -> Option<[[f32; 3]; 3]> {
    let determinant = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
        - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
    if determinant.abs() < 1e-9 {
        return None;
    }
    let inverse = 1.0 / determinant;
    Some([
        [
            (m[1][1] * m[2][2] - m[1][2] * m[2][1]) * inverse,
            (m[0][2] * m[2][1] - m[0][1] * m[2][2]) * inverse,
            (m[0][1] * m[1][2] - m[0][2] * m[1][1]) * inverse,
        ],
        [
            (m[1][2] * m[2][0] - m[1][0] * m[2][2]) * inverse,
            (m[0][0] * m[2][2] - m[0][2] * m[2][0]) * inverse,
            (m[0][2] * m[1][0] - m[0][0] * m[1][2]) * inverse,
        ],
        [
            (m[1][0] * m[2][1] - m[1][1] * m[2][0]) * inverse,
            (m[0][1] * m[2][0] - m[0][0] * m[2][1]) * inverse,
            (m[0][0] * m[1][1] - m[0][1] * m[1][0]) * inverse,
        ],
    ])
}

#[cfg(test)]
mod tests {

    #[test]
    fn sdr_white_reaches_an_hdr_screen_as_sdr_white() {
        // PQ is absolute — its 1.0 is 10,000 cd/m² — and sRGB is relative to a
        // reference white of 203. Handing PQ a relative 1.0 asks the panel for
        // ten thousand nits of white, which is every pixel at the limit: the
        // washed-out picture an HDR output showed for an ordinary desktop.
        let sdr = Description::default();
        let hdr = Description {
            transfer: TransferFunction::Pq,
            primaries: Primaries::BT2020,
            reference_luminance: 203.0,
        };

        let white = sdr.convert(&hdr, [1.0, 1.0, 1.0]);
        // 203 cd/m² is 2.03% of PQ's peak, which encodes near 0.58.
        assert!(
            (white[0] - 0.580).abs() < 0.01,
            "sdr white encoded as {} rather than about 0.58",
            white[0]
        );
        // And not the top of the range, which is what it was.
        assert!(white[0] < 0.9, "sdr white is being sent as peak brightness");

        // Black stays black.
        let black = sdr.convert(&hdr, [0.0, 0.0, 0.0]);
        assert!(black[0] < 0.001, "black came out at {}", black[0]);
    }

    #[test]
    fn hdr_content_on_an_sdr_screen_is_not_multiplied_by_ten_thousand() {
        // The other direction: PQ's 1.0 is 10,000 nits and an SDR screen's is
        // 203, so the same value has to come back down.
        let hdr = Description {
            transfer: TransferFunction::Pq,
            primaries: Primaries::BT2020,
            reference_luminance: 203.0,
        };
        assert_eq!(luminance_scale(&hdr, &Description::default()), 10_000.0 / 203.0);
    }

    #[test]
    fn two_relative_descriptions_only_compare_their_reference_whites() {
        let dim = Description {
            reference_luminance: 100.0,
            ..Description::default()
        };
        let bright = Description::default();
        assert!((luminance_scale(&dim, &bright) - 100.0 / 203.0).abs() < 1e-6);
        assert_eq!(luminance_scale(&bright, &bright), 1.0);
    }

    use super::*;

    fn close(a: f32, b: f32, tolerance: f32) -> bool {
        (a - b).abs() < tolerance
    }

    #[test]
    fn every_transfer_function_round_trips() {
        for transfer in [
            TransferFunction::Linear,
            TransferFunction::Srgb,
            TransferFunction::Gamma22,
            TransferFunction::Gamma28,
            TransferFunction::Pq,
            TransferFunction::Hlg,
        ] {
            for value in [0.0, 0.01, 0.1, 0.25, 0.5, 0.75, 0.9, 1.0] {
                let round = transfer.from_linear(transfer.to_linear(value));
                assert!(
                    close(round, value, 1e-3),
                    "{transfer:?}: {value} became {round}"
                );
            }
        }
    }

    #[test]
    fn srgb_matches_its_known_values() {
        // The mid-grey everyone quotes: 0.5 encoded is about 21.4% of light.
        assert!(close(TransferFunction::Srgb.to_linear(0.5), 0.2140, 1e-3));
        // Black and white are exact, which the piecewise segments must not
        // disturb.
        assert!(close(TransferFunction::Srgb.to_linear(0.0), 0.0, 1e-6));
        assert!(close(TransferFunction::Srgb.to_linear(1.0), 1.0, 1e-5));
    }

    #[test]
    fn srgb_is_not_a_pure_2_2_power() {
        // A common shortcut, and wrong near black — which is exactly where the
        // linear segment exists and where banding shows.
        let value = 0.02;
        let piecewise = TransferFunction::Srgb.to_linear(value);
        let power = TransferFunction::Gamma22.to_linear(value);
        assert!(
            (piecewise - power).abs() > 1e-4,
            "the two agreed at {value}, so the linear segment is missing"
        );
    }

    #[test]
    fn pq_encodes_absolute_luminance() {
        // ST 2084's whole point: 1.0 is 10000 cd/m², and the reference white
        // used for SDR sits far below it.
        let peak = TransferFunction::Pq.to_linear(1.0);
        assert!(close(peak, 1.0, 1e-3), "peak was {peak}");
        // 100 cd/m² is 1% of the peak, which lands around 0.51 encoded.
        let encoded = TransferFunction::Pq.from_linear(0.01);
        assert!(
            (0.4..0.6).contains(&encoded),
            "100 cd/m2 encoded as {encoded}"
        );
    }

    #[test]
    fn a_colour_space_converted_to_itself_is_unchanged() {
        for primaries in [
            Primaries::SRGB,
            Primaries::BT2020,
            Primaries::DISPLAY_P3,
            Primaries::ADOBE_RGB,
        ] {
            let matrix = primaries.convert_to(&primaries);
            assert_eq!(matrix, IDENTITY, "{primaries:?} did not convert to itself");
        }
    }

    #[test]
    fn white_stays_white_across_gamuts() {
        // The defining property: equal RGB is the white point, and both spaces
        // here are D65, so white must survive the conversion exactly.
        for (from, to) in [
            (Primaries::BT2020, Primaries::SRGB),
            (Primaries::SRGB, Primaries::BT2020),
            (Primaries::DISPLAY_P3, Primaries::SRGB),
        ] {
            let matrix = from.convert_to(&to);
            let white = multiply_vector(&matrix, [1.0, 1.0, 1.0]);
            for channel in white {
                assert!(close(channel, 1.0, 1e-3), "white became {white:?}");
            }
        }
    }

    #[test]
    fn srgb_to_xyz_matches_the_published_matrix() {
        // The BT.709 D65 matrix, which is tabulated in the standard — so this
        // catches an error in the construction rather than only in its
        // self-consistency.
        let m = Primaries::SRGB.to_xyz();
        let expected = [
            [0.4124, 0.3576, 0.1805],
            [0.2126, 0.7152, 0.0722],
            [0.0193, 0.1192, 0.9505],
        ];
        for (row, want) in m.iter().zip(expected.iter()) {
            for (got, want) in row.iter().zip(want.iter()) {
                assert!(close(*got, *want, 2e-3), "{m:?} != {expected:?}");
            }
        }
    }

    #[test]
    fn a_wide_gamut_red_leaves_the_srgb_range() {
        // BT.2020 red is more saturated than sRGB can represent, so converting
        // it must produce a negative channel rather than something plausible.
        let matrix = Primaries::BT2020.convert_to(&Primaries::SRGB);
        let red = multiply_vector(&matrix, [1.0, 0.0, 0.0]);
        assert!(
            red[1] < 0.0 || red[2] < 0.0,
            "BT.2020 red fitted inside sRGB, which it cannot: {red:?}"
        );
    }

    #[test]
    fn converting_between_identical_descriptions_changes_nothing() {
        let description = Description::default();
        for value in [0.0, 0.25, 0.5, 1.0] {
            let out = description.convert(&description, [value; 3]);
            for channel in out {
                assert!(close(channel, value, 1e-4), "{value} became {out:?}");
            }
        }
    }

    #[test]
    fn a_dimmer_reference_white_is_scaled_up() {
        // A surface authored against 100 cd/m2 shown on an output referenced
        // to 203 has to be scaled, or it sits too dark next to everything else.
        let dim = Description {
            reference_luminance: 100.0,
            ..Default::default()
        };
        let bright = Description {
            reference_luminance: 203.0,
            ..Default::default()
        };
        let out = dim.convert(&bright, [0.5, 0.5, 0.5]);
        assert!(
            out[0] < 0.5,
            "a dimmer reference should encode lower, got {out:?}"
        );
    }

    #[test]
    fn transfer_codes_are_distinct() {
        // The shader switches on these, so a collision would silently apply
        // the wrong curve.
        let codes: Vec<u32> = [
            TransferFunction::Linear,
            TransferFunction::Srgb,
            TransferFunction::Gamma22,
            TransferFunction::Gamma28,
            TransferFunction::Pq,
            TransferFunction::Hlg,
        ]
        .into_iter()
        .map(TransferFunction::as_code)
        .collect();
        let mut sorted = codes.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), codes.len(), "duplicate transfer codes");
    }
}
