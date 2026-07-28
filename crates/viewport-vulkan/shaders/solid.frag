// SPDX-License-Identifier: MIT
//
// A flat colour. Used for the background, for the areas no surface covers,
// and for anything the compositor draws itself.
//
// No colour conversion: a colour the compositor chose is already expressed in
// the output's space, so there is nothing to convert from.
//
// Compile with:
//   glslangValidator -V solid.frag -o solid.frag.spv

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

layout(location = 0) in vec2 in_uv;
layout(location = 0) out vec4 out_color;

void main() {
    // Premultiplied throughout: Wayland buffers are premultiplied, and mixing
    // conventions inside one pass is how edges end up with dark fringes.
    // Scaling all four components keeps it premultiplied.
    out_color = push.color * push.misc.x;
}
