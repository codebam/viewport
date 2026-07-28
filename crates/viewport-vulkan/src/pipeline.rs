// SPDX-License-Identifier: MIT
//
// Pipelines and shaders.
//
// A compositor draws rectangles: a solid one for the background and anything
// it draws itself, and a textured one per surface. That is the whole pipeline
// inventory. There are no vertex buffers — the quad comes out of
// `gl_VertexIndex` — so drawing a surface is a push constant and a draw call
// with nothing to allocate or synchronise.
//
// SPIR-V is committed rather than compiled at build time. The shaders are
// three files that change rarely, and compiling them would put a C++ toolchain
// and shaderc in the dependency graph of everyone who builds this. Regenerate
// with the glslangValidator line in each shader's header comment.

use std::collections::HashMap;
use std::ffi::CStr;

use anyhow::{Context as _, Result};
use ash::vk;

use crate::Device;

const QUAD_VERT: &[u8] = include_bytes!("../shaders/quad.vert.spv");
const SOLID_FRAG: &[u8] = include_bytes!("../shaders/solid.frag.spv");
const TEXTURE_FRAG: &[u8] = include_bytes!("../shaders/texture.frag.spv");

/// The push constant block, laid out to match `shaders/common.glsl`.
///
/// Exactly 128 bytes, which is the largest block every Vulkan implementation
/// is required to support. That budget is why the texture coordinate origin
/// rides in `pos_b`'s spare half rather than having a `vec4` of its own: the
/// colour matrix needs three whole `vec4`s and there was nowhere else to take
/// them from.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Push {
    /// Unit quad corner to clip space, the two basis vectors.
    pub pos_a: [f32; 4],
    /// `xy`: position origin. `zw`: texture coordinate origin.
    pub pos_b: [f32; 4],
    /// Corner to texture coordinate, the two basis vectors.
    pub tex_a: [f32; 4],
    /// Premultiplied colour, or a tint for the textured pipeline.
    pub color: [f32; 4],
    /// `x`: alpha. `y`: source transfer. `z`: destination transfer.
    /// `w`: relative luminance scale.
    pub misc: [f32; 4],
    /// Source primaries to destination primaries, by row. `csc0.w` carries
    /// the ignore-alpha flag; the other two fourth components are padding.
    pub csc0: [f32; 4],
    pub csc1: [f32; 4],
    pub csc2: [f32; 4],
}

impl Push {
    /// Build with no colour conversion — what the solid pipeline wants, and
    /// what a texture already in the output's space wants too.
    pub fn new(
        position: crate::transform::Affine,
        texture: crate::transform::Affine,
        color: [f32; 4],
        alpha: f32,
    ) -> Self {
        let linear = crate::color::TransferFunction::Linear.as_code() as f32;
        Self {
            pos_a: position.a,
            // The texture origin rides in the spare half.
            pos_b: [position.b[0], position.b[1], texture.b[0], texture.b[1]],
            tex_a: texture.a,
            color,
            misc: [alpha, linear, linear, 1.0],
            csc0: [1.0, 0.0, 0.0, 0.0],
            csc1: [0.0, 1.0, 0.0, 0.0],
            csc2: [0.0, 0.0, 1.0, 0.0],
        }
    }

    /// Add a colour space conversion from `from` to `to`.
    pub fn with_color(
        mut self,
        from: &crate::color::Description,
        to: &crate::color::Description,
    ) -> Self {
        use crate::color::TransferFunction;

        let matrix = from.primaries.convert_to(&to.primaries);
        self.misc[1] = from.transfer.as_code() as f32;
        self.misc[2] = to.transfer.as_code() as f32;
        // PQ carries absolute luminance of its own, so applying a relative
        // scale alongside it would count the same thing twice.
        self.misc[3] = if from.transfer == TransferFunction::Pq
            || to.transfer == TransferFunction::Pq
        {
            1.0
        } else {
            from.reference_luminance / to.reference_luminance
        };
        self.csc0 = [matrix[0][0], matrix[0][1], matrix[0][2], 0.0];
        self.csc1 = [matrix[1][0], matrix[1][1], matrix[1][2], 0.0];
        self.csc2 = [matrix[2][0], matrix[2][1], matrix[2][2], 0.0];
        self
    }

    /// Take the buffer's alpha channel as fully opaque.
    ///
    /// An X-format buffer — XRGB8888 and friends — has no alpha, and Vulkan
    /// has no X formats, so it is imported as the matching A format and the
    /// undefined byte is sampled as alpha. Clients leave it zero, so a window
    /// that declared itself opaque comes out completely transparent and only
    /// the bytes that happened to be non-zero survive.
    pub fn with_opaque(mut self, opaque: bool) -> Self {
        self.csc0[3] = if opaque { 1.0 } else { 0.0 };
        self
    }

    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY: `Push` is repr(C) and entirely f32, so it has no padding
        // holes and no invalid bit patterns.
        unsafe {
            std::slice::from_raw_parts(
                self as *const Self as *const u8,
                std::mem::size_of::<Self>(),
            )
        }
    }
}

/// Which pipeline a draw uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Solid,
    Texture,
}

/// Shader modules, the sampler, and the layouts — everything that does not
/// depend on the format being rendered into.
pub struct Pipelines {
    device: Device,
    vertex: vk::ShaderModule,
    solid: vk::ShaderModule,
    texture: vk::ShaderModule,

    sampler: vk::Sampler,
    set_layout: vk::DescriptorSetLayout,
    layout: vk::PipelineLayout,

    /// Pipelines are tied to the format they render into, and a compositor
    /// renders into whatever its outputs and clients happen to use. Built on
    /// demand and kept, because there are only ever a handful of formats.
    by_format: HashMap<(vk::Format, Kind), vk::Pipeline>,
}

impl Pipelines {
    pub fn new(device: &Device) -> Result<Self> {
        let handle = device.handle();

        let module = |bytes: &[u8]| -> Result<vk::ShaderModule> {
            // SPIR-V is a stream of u32 and the create info wants it as such.
            // include_bytes! gives a byte slice whose alignment is only 1, so
            // it has to be copied rather than cast.
            anyhow::ensure!(bytes.len() % 4 == 0, "SPIR-V is not a whole number of words");
            let words: Vec<u32> = bytes
                .chunks_exact(4)
                .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect();
            let info = vk::ShaderModuleCreateInfo::default().code(&words);
            unsafe { handle.create_shader_module(&info, None) }.context("vkCreateShaderModule")
        };

        let vertex = module(QUAD_VERT)?;
        let solid = module(SOLID_FRAG)?;
        let texture = module(TEXTURE_FRAG)?;

        // Linear filtering because surfaces get scaled — the overview draws
        // every window shrunk. CLAMP_TO_EDGE so sampling at the very edge of a
        // scaled surface does not wrap round to the opposite side.
        let sampler_info = vk::SamplerCreateInfo::default()
            .mag_filter(vk::Filter::LINEAR)
            .min_filter(vk::Filter::LINEAR)
            .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE);
        let sampler = unsafe { handle.create_sampler(&sampler_info, None) }
            .context("vkCreateSampler")?;

        let binding = vk::DescriptorSetLayoutBinding::default()
            .binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::FRAGMENT);
        let bindings = [binding];
        // PUSH_DESCRIPTOR: the surface texture is pushed straight into the
        // command buffer, so there is no descriptor pool to size, allocate
        // from, or recycle across frames.
        let set_layout_info = vk::DescriptorSetLayoutCreateInfo::default()
            .flags(vk::DescriptorSetLayoutCreateFlags::PUSH_DESCRIPTOR_KHR)
            .bindings(&bindings);
        let set_layout = unsafe { handle.create_descriptor_set_layout(&set_layout_info, None) }
            .context("vkCreateDescriptorSetLayout")?;

        let push_range = vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT)
            .offset(0)
            .size(std::mem::size_of::<Push>() as u32);
        let set_layouts = [set_layout];
        let ranges = [push_range];
        let layout_info = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(&set_layouts)
            .push_constant_ranges(&ranges);
        let layout = unsafe { handle.create_pipeline_layout(&layout_info, None) }
            .context("vkCreatePipelineLayout")?;

        Ok(Self {
            device: device.clone(),
            vertex,
            solid,
            texture,
            sampler,
            set_layout,
            layout,
            by_format: HashMap::new(),
        })
    }

    pub fn layout(&self) -> vk::PipelineLayout {
        self.layout
    }

    pub fn sampler(&self) -> vk::Sampler {
        self.sampler
    }

    /// The pipeline for this kind of draw into this format, building it if it
    /// has not been needed before.
    pub fn get(&mut self, format: vk::Format, kind: Kind) -> Result<vk::Pipeline> {
        if let Some(pipeline) = self.by_format.get(&(format, kind)) {
            return Ok(*pipeline);
        }
        let pipeline = self.build(format, kind)?;
        self.by_format.insert((format, kind), pipeline);
        Ok(pipeline)
    }

    fn build(&self, format: vk::Format, kind: Kind) -> Result<vk::Pipeline> {
        let handle = self.device.handle();
        let entry = CStr::from_bytes_with_nul(b"main\0").unwrap();

        let fragment = match kind {
            Kind::Solid => self.solid,
            Kind::Texture => self.texture,
        };
        let stages = [
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::VERTEX)
                .module(self.vertex)
                .name(entry),
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(fragment)
                .name(entry),
        ];

        // No vertex buffers at all: the quad is generated in the shader.
        let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();
        let assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
            .topology(vk::PrimitiveTopology::TRIANGLE_STRIP);

        // Both are dynamic, set per frame from the render area.
        let viewport_state = vk::PipelineViewportStateCreateInfo::default()
            .viewport_count(1)
            .scissor_count(1);
        let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dynamic = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);

        let rasterisation = vk::PipelineRasterizationStateCreateInfo::default()
            .polygon_mode(vk::PolygonMode::FILL)
            // Quads are drawn in a fixed winding and never seen from behind.
            .cull_mode(vk::CullModeFlags::NONE)
            .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
            .line_width(1.0);

        let multisample = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(vk::SampleCountFlags::TYPE_1);

        // Premultiplied alpha: ONE, not SRC_ALPHA. Wayland buffers are
        // premultiplied, and using SRC_ALPHA here double-multiplies, which
        // looks like dark halos around translucent edges.
        let blend_attachment = vk::PipelineColorBlendAttachmentState::default()
            .blend_enable(true)
            .src_color_blend_factor(vk::BlendFactor::ONE)
            .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .color_blend_op(vk::BlendOp::ADD)
            .src_alpha_blend_factor(vk::BlendFactor::ONE)
            .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .alpha_blend_op(vk::BlendOp::ADD)
            .color_write_mask(vk::ColorComponentFlags::RGBA);
        let attachments = [blend_attachment];
        let blend = vk::PipelineColorBlendStateCreateInfo::default().attachments(&attachments);

        // Dynamic rendering: the pipeline is told the attachment format
        // directly instead of being tied to a VkRenderPass.
        let formats = [format];
        let mut rendering =
            vk::PipelineRenderingCreateInfo::default().color_attachment_formats(&formats);

        let info = vk::GraphicsPipelineCreateInfo::default()
            .stages(&stages)
            .vertex_input_state(&vertex_input)
            .input_assembly_state(&assembly)
            .viewport_state(&viewport_state)
            .rasterization_state(&rasterisation)
            .multisample_state(&multisample)
            .color_blend_state(&blend)
            .dynamic_state(&dynamic)
            .layout(self.layout)
            .push_next(&mut rendering);

        let pipelines = unsafe {
            handle.create_graphics_pipelines(vk::PipelineCache::null(), &[info], None)
        }
        .map_err(|(_, e)| anyhow::Error::from(e).context("vkCreateGraphicsPipelines"))?;

        Ok(pipelines[0])
    }
}

impl std::fmt::Debug for Pipelines {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pipelines")
            .field("built", &self.by_format.len())
            .finish()
    }
}

impl Drop for Pipelines {
    fn drop(&mut self) {
        let handle = self.device.handle();
        unsafe {
            let _ = handle.device_wait_idle();
            for pipeline in self.by_format.values() {
                handle.destroy_pipeline(*pipeline, None);
            }
            handle.destroy_pipeline_layout(self.layout, None);
            handle.destroy_descriptor_set_layout(self.set_layout, None);
            handle.destroy_sampler(self.sampler, None);
            handle.destroy_shader_module(self.vertex, None);
            handle.destroy_shader_module(self.solid, None);
            handle.destroy_shader_module(self.texture, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_push_block_matches_the_shader_layout() {
        // Eight vec4s at 16-byte intervals, as the GLSL declares them. If this
        // drifts, geometry lands in the wrong place and nothing reports an
        // error.
        assert_eq!(std::mem::offset_of!(Push, pos_a), 0);
        assert_eq!(std::mem::offset_of!(Push, pos_b), 16);
        assert_eq!(std::mem::offset_of!(Push, tex_a), 32);
        assert_eq!(std::mem::offset_of!(Push, color), 48);
        assert_eq!(std::mem::offset_of!(Push, misc), 64);
        assert_eq!(std::mem::offset_of!(Push, csc0), 80);
        assert_eq!(std::mem::offset_of!(Push, csc1), 96);
        assert_eq!(std::mem::offset_of!(Push, csc2), 112);
        // Exactly the minimum every implementation must provide. Adding
        // anything means finding somewhere else to put it.
        assert_eq!(std::mem::size_of::<Push>(), 128);
    }

    #[test]
    fn the_shaders_are_spirv() {
        // 0x07230203 is the SPIR-V magic number. Catches a truncated or
        // wrong-endian .spv checked into the repository.
        for (name, bytes) in [
            ("quad.vert", QUAD_VERT),
            ("solid.frag", SOLID_FRAG),
            ("texture.frag", TEXTURE_FRAG),
        ] {
            assert!(bytes.len() % 4 == 0, "{name} is not a whole number of words");
            let magic = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            assert_eq!(magic, 0x0723_0203, "{name} is not SPIR-V");
        }
    }
}
