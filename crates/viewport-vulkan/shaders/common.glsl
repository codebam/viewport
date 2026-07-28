// SPDX-License-Identifier: MIT
//
// The push constant block, shared by all three shaders.
//
// Exactly 128 bytes, which is the largest block every Vulkan implementation
// is required to support. That budget is why the texture coordinate origin
// rides in pos_b.zw rather than having a vec4 of its own — the colour matrix
// needs three whole vec4s and there was nowhere else to take them from.

layout(push_constant) uniform Push {
    // Unit quad corner to clip space: pos_a holds the images of (1,0) and
    // (0,1) relative to the origin, pos_b.xy holds the origin.
    vec4 pos_a;
    // xy: position origin. zw: texture coordinate origin.
    vec4 pos_b;
    // Corner to texture coordinate, the two basis vectors.
    vec4 tex_a;
    // Premultiplied colour, or a tint for the textured pipeline.
    vec4 color;
    // x: alpha. y: source transfer function. z: destination transfer
    // function. w: relative luminance scale.
    vec4 misc;
    // Linear source primaries to linear destination primaries, by row.
    vec4 csc0;
    vec4 csc1;
    vec4 csc2;
} push;

// Must match TransferFunction::as_code in color.rs. A mismatch applies the
// wrong curve and looks like a washed-out or crushed image rather than an
// error.
const uint TRANSFER_LINEAR = 0u;
const uint TRANSFER_SRGB = 1u;
const uint TRANSFER_GAMMA22 = 2u;
const uint TRANSFER_GAMMA28 = 3u;
const uint TRANSFER_PQ = 4u;
const uint TRANSFER_HLG = 5u;
