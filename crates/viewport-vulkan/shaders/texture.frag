// SPDX-License-Identifier: MIT
//
// A surface, converted from its own colour space into the output's.
//
// Three steps, and none of them is optional if HDR and SDR surfaces are to
// sit next to each other correctly: decode the encoded value to light, move
// it from the surface's primaries into the output's, and re-encode. Skipping
// the middle step is what makes wide-gamut content look oversaturated, and
// skipping the first is what makes it look washed out.
//
// Every function here is a translation of one in color.rs, which is checked
// against published values on the CPU.
//
// Compile with:
//   glslangValidator -V texture.frag -o texture.frag.spv

#version 450

layout(push_constant) uniform Push {
    vec4 pos_a;
    vec4 pos_b;
    vec4 tex_a;
    vec4 color;
    vec4 misc;
    vec4 csc0;
    vec4 csc1;
    vec4 csc2;
} push;

const uint TRANSFER_LINEAR = 0u;
const uint TRANSFER_SRGB = 1u;
const uint TRANSFER_GAMMA22 = 2u;
const uint TRANSFER_GAMMA28 = 3u;
const uint TRANSFER_PQ = 4u;
const uint TRANSFER_HLG = 5u;

layout(set = 0, binding = 0) uniform sampler2D surface;

layout(location = 0) in vec2 in_uv;
layout(location = 0) out vec4 out_color;

vec3 to_linear(vec3 v, uint transfer) {
    if (transfer == TRANSFER_LINEAR) {
        return v;
    }
    if (transfer == TRANSFER_SRGB) {
        // Piecewise, not a 2.2 power. The linear segment near black is the
        // difference, and it is where banding would show.
        bvec3 low = lessThanEqual(v, vec3(0.04045));
        vec3 lo = v / 12.92;
        vec3 hi = pow(max((v + 0.055) / 1.055, vec3(0.0)), vec3(2.4));
        return mix(hi, lo, vec3(low));
    }
    if (transfer == TRANSFER_GAMMA22) {
        return pow(max(v, vec3(0.0)), vec3(2.2));
    }
    if (transfer == TRANSFER_GAMMA28) {
        return pow(max(v, vec3(0.0)), vec3(2.8));
    }
    if (transfer == TRANSFER_PQ) {
        const float m1 = 0.15930176;
        const float m2 = 78.84375;
        const float c1 = 0.8359375;
        const float c2 = 18.8515625;
        const float c3 = 18.6875;

        vec3 powed = pow(clamp(v, vec3(0.0), vec3(1.0)), vec3(1.0 / m2));
        vec3 numerator = max(powed - c1, vec3(0.0));
        vec3 denominator = max(c2 - c3 * powed, vec3(1e-6));
        return pow(numerator / denominator, vec3(1.0 / m1));
    }
    // HLG.
    const float a = 0.17883277;
    const float b = 0.28466892;
    const float c = 0.55991073;
    vec3 clamped = clamp(v, vec3(0.0), vec3(1.0));
    bvec3 low = lessThanEqual(clamped, vec3(0.5));
    vec3 lo = (clamped * clamped) / 3.0;
    vec3 hi = (exp((clamped - c) / a) + b) / 12.0;
    return mix(hi, lo, vec3(low));
}

vec3 from_linear(vec3 v, uint transfer) {
    if (transfer == TRANSFER_LINEAR) {
        return v;
    }
    if (transfer == TRANSFER_SRGB) {
        bvec3 low = lessThanEqual(v, vec3(0.0031308));
        vec3 lo = v * 12.92;
        vec3 hi = 1.055 * pow(max(v, vec3(0.0)), vec3(1.0 / 2.4)) - 0.055;
        return mix(hi, lo, vec3(low));
    }
    if (transfer == TRANSFER_GAMMA22) {
        return pow(max(v, vec3(0.0)), vec3(1.0 / 2.2));
    }
    if (transfer == TRANSFER_GAMMA28) {
        return pow(max(v, vec3(0.0)), vec3(1.0 / 2.8));
    }
    if (transfer == TRANSFER_PQ) {
        const float m1 = 0.15930176;
        const float m2 = 78.84375;
        const float c1 = 0.8359375;
        const float c2 = 18.8515625;
        const float c3 = 18.6875;

        vec3 powed = pow(max(v, vec3(0.0)), vec3(m1));
        return pow((c1 + c2 * powed) / (1.0 + c3 * powed), vec3(m2));
    }
    // HLG.
    const float a = 0.17883277;
    const float b = 0.28466892;
    const float c = 0.55991073;
    vec3 nonneg = max(v, vec3(0.0));
    bvec3 low = lessThanEqual(nonneg, vec3(1.0 / 12.0));
    vec3 lo = sqrt(3.0 * nonneg);
    vec3 hi = a * log(max(12.0 * nonneg - b, vec3(1e-6))) + c;
    return mix(hi, lo, vec3(low));
}

void main() {
    vec4 texel = texture(surface, in_uv);

    uint src_transfer = uint(push.misc.y + 0.5);
    uint dst_transfer = uint(push.misc.z + 0.5);

    // Wayland buffers are premultiplied, and every step below is defined on
    // unpremultiplied light. Dividing out first and multiplying back at the
    // end is the difference between correct edges and dark fringes.
    float a = texel.a;
    vec3 rgb = a > 0.0 ? texel.rgb / a : texel.rgb;

    vec3 linear = to_linear(rgb, src_transfer);

    mat3 primaries = mat3(
        vec3(push.csc0.x, push.csc1.x, push.csc2.x),
        vec3(push.csc0.y, push.csc1.y, push.csc2.y),
        vec3(push.csc0.z, push.csc1.z, push.csc2.z));
    linear = primaries * linear;

    // A surface authored against a dimmer reference white has to be scaled to
    // sit correctly beside one that was not.
    linear *= push.misc.w;

    // Out-of-gamut values are real: a saturated BT.2020 red has no sRGB
    // representation and comes out negative. Clamping is the crude answer;
    // proper gamut mapping is a separate problem and pretending it is solved
    // here would be worse than clipping visibly.
    linear = max(linear, vec3(0.0));

    vec3 encoded = from_linear(linear, dst_transfer);

    out_color = vec4(encoded * a, a) * push.color * push.misc.x;
}
