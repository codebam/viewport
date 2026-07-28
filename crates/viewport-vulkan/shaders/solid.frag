// SPDX-License-Identifier: MIT
//
// A flat colour. Used for the background, for the areas no surface covers,
// and for anything the compositor draws itself.
//
// Compile with:
//   glslangValidator -V solid.frag -o solid.frag.spv

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

layout(location = 0) in vec2 in_uv;
layout(location = 0) out vec4 out_color;

void main() {
    // Premultiplied throughout: Wayland buffers are premultiplied, and mixing
    // conventions inside one pass is how edges end up with dark fringes.
    // Scaling all four components keeps it premultiplied.
    out_color = push.color * push.misc.x;
}
