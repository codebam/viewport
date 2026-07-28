// SPDX-License-Identifier: MIT
//
// A surface. Samples the client's buffer, imported as a DMA-BUF.
//
// Colour management will land here: this is where an imported image's transfer
// function and primaries get applied on the way to the output's colour space.
// For now it is a straight sample, which is correct only while everything is
// sRGB.
//
// Compile with:
//   glslangValidator -V texture.frag -o texture.frag.spv

#version 450

layout(push_constant) uniform Push {
    vec4 pos_a;
    vec4 pos_b;
    vec4 tex_a;
    vec4 tex_b;
    vec4 color;
    // x is alpha.
    vec4 misc;
} push;

layout(set = 0, binding = 0) uniform sampler2D surface;

layout(location = 0) in vec2 in_uv;
layout(location = 0) out vec4 out_color;

void main() {
    vec4 texel = texture(surface, in_uv);

    // `color` is a tint, defaulting to white. Multiplying a premultiplied
    // texel by a scalar alpha keeps it premultiplied, which is what the
    // ONE / ONE_MINUS_SRC_ALPHA blend this is drawn with expects.
    out_color = texel * push.color * push.misc.x;
}
