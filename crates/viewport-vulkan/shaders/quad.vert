// SPDX-License-Identifier: MIT
//
// The only vertex shader this renderer has.
//
// A compositor draws rectangles and nothing else, so there are no vertex
// buffers: the quad is generated from gl_VertexIndex as a 4-vertex triangle
// strip, and where it lands comes entirely from push constants. That means
// drawing a surface is a push and a draw, with no buffer to allocate, map or
// synchronise.
//
// Compile with:
//   glslangValidator -V quad.vert -o quad.vert.spv

#version 450

layout(push_constant) uniform Push {
    // Destination rectangle in target pixels: x, y, width, height.
    vec4 dst;
    // Source rectangle in normalised texture coordinates: u, v, width, height.
    vec4 src;
    // Solid colour, or a tint for the textured pipeline. Premultiplied.
    vec4 color;
    // Size of the render target in pixels, to turn dst into clip space.
    vec2 target;
    float alpha;
} push;

layout(location = 0) out vec2 out_uv;

void main() {
    // 0 -> (0,0), 1 -> (1,0), 2 -> (0,1), 3 -> (1,1). A triangle strip over
    // these four corners is one quad with no index buffer.
    vec2 corner = vec2(float(gl_VertexIndex & 1), float((gl_VertexIndex >> 1) & 1));

    vec2 pixel = push.dst.xy + corner * push.dst.zw;

    // Pixels to clip space. No Y flip: unlike OpenGL, Vulkan's clip space has
    // +Y pointing down, the same direction as framebuffer coordinates, so the
    // mapping is direct. Adding a flip here is the classic way to end up with
    // every surface upside down.
    vec2 ndc = (pixel / push.target) * 2.0 - 1.0;

    gl_Position = vec4(ndc, 0.0, 1.0);
    out_uv = push.src.xy + corner * push.src.zw;
}
