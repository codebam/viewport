// SPDX-License-Identifier: MIT
//
// The only vertex shader this renderer has.
//
// A compositor draws rectangles and nothing else, so there are no vertex
// buffers: the quad is generated from gl_VertexIndex as a 4-vertex triangle
// strip, and everything about where it lands and what it samples arrives as
// two affine maps in push constants.
//
// The maps are built on the CPU in transform.rs, which is why there is no
// rotation, flipping or source-rectangle arithmetic here. All of that is
// testable without a GPU, and this stays four multiply-adds.
//
// Compile with:
//   glslangValidator -V quad.vert -o quad.vert.spv

#version 450

layout(push_constant) uniform Push {
    // Unit quad corner to clip space. pos_a holds the images of (1,0) and
    // (0,1) relative to the origin; pos_b holds the origin.
    vec4 pos_a;
    vec4 pos_b;
    // The same, for texture coordinates.
    vec4 tex_a;
    vec4 tex_b;
    // Premultiplied colour, or a tint for the textured pipeline.
    vec4 color;
    // x is alpha; the rest is padding to keep the block 16-byte aligned.
    vec4 misc;
} push;

layout(location = 0) out vec2 out_uv;

void main() {
    // 0 -> (0,0), 1 -> (1,0), 2 -> (0,1), 3 -> (1,1). A triangle strip over
    // these four corners is one quad with no index buffer.
    vec2 corner = vec2(float(gl_VertexIndex & 1), float((gl_VertexIndex >> 1) & 1));

    vec2 position = push.pos_a.xy * corner.x + push.pos_a.zw * corner.y + push.pos_b.xy;
    gl_Position = vec4(position, 0.0, 1.0);

    out_uv = push.tex_a.xy * corner.x + push.tex_a.zw * corner.y + push.tex_b.xy;
}
