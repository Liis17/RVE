//! Графические и вычислительные конвейеры, shader modules и pipeline cache.

use crate::*;

mod scene;
mod screen;

#[derive(Clone, Copy)]
pub(crate) struct PipelineCacheIdentity {
    pub(crate) vendor_id: u32,
    pub(crate) device_id: u32,
    pub(crate) pipeline_cache_uuid: [u8; vk::UUID_SIZE],
}

pub(crate) fn validate_pipeline_cache(
    data: &[u8],
    expected: PipelineCacheIdentity,
) -> Result<(), &'static str> {
    if data.is_empty() {
        return Ok(());
    }
    if data.len() < PIPELINE_CACHE_HEADER_SIZE {
        return Err("pipeline cache header is truncated");
    }

    let header_size = u32::from_ne_bytes(data[0..4].try_into().unwrap()) as usize;
    if header_size != PIPELINE_CACHE_HEADER_SIZE {
        return Err("pipeline cache header size is invalid");
    }
    if data.len() < header_size {
        return Err("pipeline cache header is truncated");
    }
    let header_version = i32::from_ne_bytes(data[4..8].try_into().unwrap());
    if header_version != vk::PipelineCacheHeaderVersion::ONE.as_raw() {
        return Err("pipeline cache header version is unsupported");
    }
    let vendor_id = u32::from_ne_bytes(data[8..12].try_into().unwrap());
    let device_id = u32::from_ne_bytes(data[12..16].try_into().unwrap());
    let uuid = &data[16..PIPELINE_CACHE_HEADER_SIZE];
    if vendor_id != expected.vendor_id
        || device_id != expected.device_id
        || uuid != expected.pipeline_cache_uuid
    {
        return Err("pipeline cache belongs to another physical device");
    }
    Ok(())
}

pub(crate) fn compatible_pipeline_cache(data: &[u8], expected: PipelineCacheIdentity) -> &[u8] {
    match validate_pipeline_cache(data, expected) {
        Ok(()) => data,
        Err(reason) => {
            eprintln!("ignoring pipeline cache: {reason}");
            &[]
        }
    }
}

pub(crate) struct ShaderModule {
    pub(crate) device: ash::Device,
    pub(crate) handle: vk::ShaderModule,
}

impl ShaderModule {
    pub(crate) fn new(device: &ash::Device, spirv: &[u8]) -> Result<Self, RenderError> {
        let code = ash::util::read_spv(&mut std::io::Cursor::new(spirv))
            .map_err(|_| RenderError::InvalidState("встроенный SPIR-V повреждён"))?;
        let info = vk::ShaderModuleCreateInfo::default().code(&code);
        // SAFETY: the SPIR-V was produced and validated by naga at build time.
        let handle = unsafe { device.create_shader_module(&info, None) }?;
        Ok(Self {
            device: device.clone(),
            handle,
        })
    }
}

impl Drop for ShaderModule {
    fn drop(&mut self) {
        // SAFETY: shader modules may be destroyed once pipeline creation has returned.
        unsafe { self.device.destroy_shader_module(self.handle, None) };
    }
}

pub(crate) fn create_compute_pipeline(
    device: &ash::Device,
    cache: vk::PipelineCache,
    layout: vk::PipelineLayout,
    compute_shader: &[u8],
) -> Result<vk::Pipeline, RenderError> {
    let module = ShaderModule::new(device, compute_shader)?;
    let stage = vk::PipelineShaderStageCreateInfo::default()
        .stage(vk::ShaderStageFlags::COMPUTE)
        .module(module.handle)
        .name(c"main");
    let info = vk::ComputePipelineCreateInfo::default()
        .stage(stage)
        .layout(layout);
    // SAFETY: shader module and pipeline layout are valid for this compute pipeline.
    let mut pipelines = match unsafe { device.create_compute_pipelines(cache, &[info], None) } {
        Ok(pipelines) => pipelines,
        Err((pipelines, error)) => {
            for pipeline in pipelines {
                // SAFETY: partial compute pipelines were returned by this device.
                unsafe { device.destroy_pipeline(pipeline, None) };
            }
            return Err(error.into());
        }
    };
    pipelines
        .pop()
        .ok_or(RenderError::InvalidState("compute pipeline не создан"))
}

pub(crate) fn create_shadow_graphics_pipeline(
    device: &ash::Device,
    cache: vk::PipelineCache,
    layout: vk::PipelineLayout,
    vertex_shader: &[u8],
) -> Result<vk::Pipeline, RenderError> {
    let vertex_module = ShaderModule::new(device, vertex_shader)?;
    let fragment_module = ShaderModule::new(device, SHADOW_FRAGMENT_SHADER)?;
    let stages = [
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::VERTEX)
            .module(vertex_module.handle)
            .name(c"main"),
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::FRAGMENT)
            .module(fragment_module.handle)
            .name(c"main"),
    ];
    let vertex_bindings = [
        vk::VertexInputBindingDescription::default()
            .binding(0)
            .stride(size_of::<Vertex>() as u32)
            .input_rate(vk::VertexInputRate::VERTEX),
        instance_binding(SHADOW_INSTANCE_BINDING),
    ];
    // Shadow читает только столбцы model в начале данных экземпляра.
    let vertex_attributes = [
        vk::VertexInputAttributeDescription::default()
            .location(0)
            .binding(0)
            .format(vk::Format::R32G32B32_SFLOAT)
            .offset(std::mem::offset_of!(Vertex, position) as u32),
        vk::VertexInputAttributeDescription::default()
            .location(1)
            .binding(0)
            .format(vk::Format::R32G32_SFLOAT)
            .offset(std::mem::offset_of!(Vertex, uv) as u32),
    ]
    .into_iter()
    .chain(instance_attributes(SHADOW_INSTANCE_BINDING, 2, 4))
    .collect::<Vec<_>>();
    let vertex_input = vk::PipelineVertexInputStateCreateInfo::default()
        .vertex_binding_descriptions(&vertex_bindings)
        .vertex_attribute_descriptions(&vertex_attributes);
    let input_assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
        .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
    let viewport_state = vk::PipelineViewportStateCreateInfo::default()
        .viewport_count(1)
        .scissor_count(1);
    let rasterization = vk::PipelineRasterizationStateCreateInfo::default()
        .depth_bias_enable(true)
        .depth_bias_constant_factor(1.25)
        .depth_bias_slope_factor(1.75)
        .polygon_mode(vk::PolygonMode::FILL)
        .cull_mode(vk::CullModeFlags::NONE)
        .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
        .line_width(1.0);
    let multisample = vk::PipelineMultisampleStateCreateInfo::default()
        .rasterization_samples(vk::SampleCountFlags::TYPE_1);
    let depth_stencil = vk::PipelineDepthStencilStateCreateInfo::default()
        .depth_test_enable(true)
        .depth_write_enable(true)
        .depth_compare_op(vk::CompareOp::LESS);
    let dynamic_states = [
        vk::DynamicState::VIEWPORT,
        vk::DynamicState::SCISSOR,
        vk::DynamicState::CULL_MODE,
        vk::DynamicState::FRONT_FACE,
    ];
    let dynamic_state =
        vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
    let mut rendering_info =
        vk::PipelineRenderingCreateInfo::default().depth_attachment_format(DEPTH_FORMAT);
    let pipeline_info = [vk::GraphicsPipelineCreateInfo::default()
        .stages(&stages)
        .vertex_input_state(&vertex_input)
        .input_assembly_state(&input_assembly)
        .viewport_state(&viewport_state)
        .rasterization_state(&rasterization)
        .multisample_state(&multisample)
        .depth_stencil_state(&depth_stencil)
        .dynamic_state(&dynamic_state)
        .layout(layout)
        .push_next(&mut rendering_info)];
    // SAFETY: shader, shared layout, depth format and dynamic-rendering state are valid.
    let mut pipelines =
        match unsafe { device.create_graphics_pipelines(cache, &pipeline_info, None) } {
            Ok(pipelines) => pipelines,
            Err((pipelines, error)) => {
                for pipeline in pipelines {
                    // SAFETY: these partial pipeline handles were returned by this device.
                    unsafe { device.destroy_pipeline(pipeline, None) };
                }
                return Err(RenderError::Vulkan(error));
            }
        };
    pipelines
        .pop()
        .ok_or(RenderError::InvalidState("shadow pipeline не создан"))
}

pub(crate) fn create_skybox_graphics_pipeline(
    device: &ash::Device,
    cache: vk::PipelineCache,
    layout: vk::PipelineLayout,
) -> Result<vk::Pipeline, RenderError> {
    let vertex_module = ShaderModule::new(device, SKYBOX_VERTEX_SHADER)?;
    let fragment_module = ShaderModule::new(device, SKYBOX_FRAGMENT_SHADER)?;
    let stages = [
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::VERTEX)
            .module(vertex_module.handle)
            .name(c"main"),
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::FRAGMENT)
            .module(fragment_module.handle)
            .name(c"main"),
    ];
    let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();
    let input_assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
        .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
    let viewport_state = vk::PipelineViewportStateCreateInfo::default()
        .viewport_count(1)
        .scissor_count(1);
    let rasterization = vk::PipelineRasterizationStateCreateInfo::default()
        .polygon_mode(vk::PolygonMode::FILL)
        .cull_mode(vk::CullModeFlags::NONE)
        .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
        .line_width(1.0);
    let multisample = vk::PipelineMultisampleStateCreateInfo::default()
        .rasterization_samples(vk::SampleCountFlags::TYPE_1);
    let depth_stencil = vk::PipelineDepthStencilStateCreateInfo::default()
        .depth_test_enable(true)
        .depth_write_enable(false)
        .depth_compare_op(vk::CompareOp::LESS_OR_EQUAL);
    let color_attachments = [
        vk::PipelineColorBlendAttachmentState::default()
            .color_write_mask(vk::ColorComponentFlags::RGBA),
        vk::PipelineColorBlendAttachmentState::default()
            .color_write_mask(vk::ColorComponentFlags::R | vk::ColorComponentFlags::G),
    ];
    let color_blend =
        vk::PipelineColorBlendStateCreateInfo::default().attachments(&color_attachments);
    let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
    let dynamic_state =
        vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
    let color_formats = [OFFSCREEN_FORMAT, VELOCITY_FORMAT];
    let mut rendering_info = vk::PipelineRenderingCreateInfo::default()
        .color_attachment_formats(&color_formats)
        .depth_attachment_format(DEPTH_FORMAT);
    let info = [vk::GraphicsPipelineCreateInfo::default()
        .stages(&stages)
        .vertex_input_state(&vertex_input)
        .input_assembly_state(&input_assembly)
        .viewport_state(&viewport_state)
        .rasterization_state(&rasterization)
        .multisample_state(&multisample)
        .depth_stencil_state(&depth_stencil)
        .color_blend_state(&color_blend)
        .dynamic_state(&dynamic_state)
        .layout(layout)
        .push_next(&mut rendering_info)];
    // SAFETY: shaders, layout, color/depth formats and dynamic rendering state are valid.
    let mut pipelines = match unsafe { device.create_graphics_pipelines(cache, &info, None) } {
        Ok(pipelines) => pipelines,
        Err((pipelines, error)) => {
            for pipeline in pipelines {
                // SAFETY: partial graphics pipelines were returned by this device.
                unsafe { device.destroy_pipeline(pipeline, None) };
            }
            return Err(error.into());
        }
    };
    pipelines
        .pop()
        .ok_or(RenderError::InvalidState("skybox pipeline не создан"))
}

pub(crate) fn create_post_pipeline(
    device: &ash::Device,
    cache: vk::PipelineCache,
    layout: vk::PipelineLayout,
    color_format: vk::Format,
    fragment_shader: &[u8],
    additive: bool,
) -> Result<vk::Pipeline, RenderError> {
    let vertex_module = ShaderModule::new(device, FULLSCREEN_VERTEX_SHADER)?;
    let fragment_module = ShaderModule::new(device, fragment_shader)?;
    let stages = [
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::VERTEX)
            .module(vertex_module.handle)
            .name(c"main"),
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::FRAGMENT)
            .module(fragment_module.handle)
            .name(c"main"),
    ];
    let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();
    let input_assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
        .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
    let viewport_state = vk::PipelineViewportStateCreateInfo::default()
        .viewport_count(1)
        .scissor_count(1);
    let rasterization = vk::PipelineRasterizationStateCreateInfo::default()
        .polygon_mode(vk::PolygonMode::FILL)
        .cull_mode(vk::CullModeFlags::NONE)
        .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
        .line_width(1.0);
    let multisample = vk::PipelineMultisampleStateCreateInfo::default()
        .rasterization_samples(vk::SampleCountFlags::TYPE_1);
    // Аддитивное смешивание нужно только upsample bloom: цель уже содержит уровень.
    let blend_attachment = if additive {
        vk::PipelineColorBlendAttachmentState::default()
            .color_write_mask(vk::ColorComponentFlags::RGBA)
            .blend_enable(true)
            .src_color_blend_factor(vk::BlendFactor::ONE)
            .dst_color_blend_factor(vk::BlendFactor::ONE)
            .color_blend_op(vk::BlendOp::ADD)
            .src_alpha_blend_factor(vk::BlendFactor::ONE)
            .dst_alpha_blend_factor(vk::BlendFactor::ONE)
            .alpha_blend_op(vk::BlendOp::ADD)
    } else {
        vk::PipelineColorBlendAttachmentState::default()
            .color_write_mask(vk::ColorComponentFlags::RGBA)
    };
    let blend_attachments = [blend_attachment];
    let color_blend =
        vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend_attachments);
    let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
    let dynamic_state =
        vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
    let color_formats = [color_format];
    let mut rendering_info =
        vk::PipelineRenderingCreateInfo::default().color_attachment_formats(&color_formats);
    let pipeline_info = [vk::GraphicsPipelineCreateInfo::default()
        .stages(&stages)
        .vertex_input_state(&vertex_input)
        .input_assembly_state(&input_assembly)
        .viewport_state(&viewport_state)
        .rasterization_state(&rasterization)
        .multisample_state(&multisample)
        .color_blend_state(&color_blend)
        .dynamic_state(&dynamic_state)
        .layout(layout)
        .push_next(&mut rendering_info)];
    // SAFETY: the modules, layout and dynamic-rendering format belong to this device.
    let mut pipelines =
        match unsafe { device.create_graphics_pipelines(cache, &pipeline_info, None) } {
            Ok(pipelines) => pipelines,
            Err((pipelines, error)) => {
                for pipeline in pipelines {
                    // SAFETY: partial pipeline handles were returned by this device.
                    unsafe { device.destroy_pipeline(pipeline, None) };
                }
                return Err(error.into());
            }
        };
    pipelines
        .pop()
        .ok_or(RenderError::InvalidState("post pipeline не создан"))
}

/// Push constants: model, normal matrix, previous model and one uvec4 of draw parameters.
/// Привязка данных экземпляров с шагом [`INSTANCE_STRIDE`] на instance.
pub(crate) fn instance_binding(binding: u32) -> vk::VertexInputBindingDescription {
    vk::VertexInputBindingDescription::default()
        .binding(binding)
        .stride(INSTANCE_STRIDE as u32)
        .input_rate(vk::VertexInputRate::INSTANCE)
}

/// Атрибуты `vec4` первых `count` векторов данных экземпляра на локациях от `first_location`.
pub(crate) fn instance_attributes(
    binding: u32,
    first_location: u32,
    count: u32,
) -> impl Iterator<Item = vk::VertexInputAttributeDescription> {
    (0..count).map(move |vector| {
        vk::VertexInputAttributeDescription::default()
            .location(first_location + vector)
            .binding(binding)
            .format(vk::Format::R32G32B32A32_SFLOAT)
            .offset(vector * size_of::<[f32; 4]>() as u32)
    })
}
