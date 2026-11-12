//! Экранные конвейеры: постобработка, overlay и UI.

use crate::*;

impl Renderer {
    pub(crate) fn create_post_pipelines(&mut self) -> Result<(), RenderError> {
        let device = self.device()?.clone();
        let cache = self.pipeline_cache()?;
        let set_layouts = [self.post_set_layout()?];
        let push_constant_ranges = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)
            .offset(0)
            .size(COMPOSITE_PUSH_CONSTANTS_SIZE as u32)];
        let layout_info = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(&set_layouts)
            .push_constant_ranges(&push_constant_ranges);
        // SAFETY: the image/sampler set layout and the composite fragment range are valid.
        let layout = unsafe { device.create_pipeline_layout(&layout_info, None) }?;
        self.post_pipeline_layout = Some(layout);
        self.tonemap_pipeline = Some(create_post_pipeline(
            &device,
            cache,
            layout,
            LDR_FORMAT,
            TONEMAP_FRAGMENT_SHADER,
            false,
        )?);
        self.output_pipeline = Some(create_post_pipeline(
            &device,
            cache,
            layout,
            self.surface_format.format,
            OUTPUT_FRAGMENT_SHADER,
            false,
        )?);
        self.viewport_output_pipeline = Some(create_post_pipeline(
            &device,
            cache,
            layout,
            LDR_FORMAT,
            OUTPUT_FRAGMENT_SHADER,
            false,
        )?);
        let taa_set_layouts = [self.taa_set_layout()?, self.frame_set_layout()?];
        let taa_push_constants = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)
            .offset(0)
            .size(TAA_PUSH_CONSTANTS_SIZE as u32)];
        let taa_layout_info = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(&taa_set_layouts)
            .push_constant_ranges(&taa_push_constants);
        // SAFETY: the TAA descriptor layouts and 16-byte fragment range match taa.frag.
        let taa_layout = unsafe { device.create_pipeline_layout(&taa_layout_info, None) }?;
        self.taa_pipeline_layout = Some(taa_layout);
        self.taa_pipeline = Some(create_post_pipeline(
            &device,
            cache,
            taa_layout,
            OFFSCREEN_FORMAT,
            TAA_FRAGMENT_SHADER,
            false,
        )?);
        self.bloom_pipelines = Some(BloomPipelines::new(
            &device,
            cache,
            self.texture_set_layout()?,
        )?);
        self.exposure_pipelines = Some(ExposurePipelines::new(&device, cache)?);
        self.filter_pipelines = Some(FilterPipelines::new(&device, cache)?);
        self.ao_pipelines = Some(AoPipelines::new(&device, cache)?);
        if self.indirect_instancing {
            self.occlusion_pipelines = Some(OcclusionPipelines::new(&device, cache)?);
        }
        Ok(())
    }

    /// Pipeline оверлея: 2D-треугольники с альфа-смешиванием прямо в swapchain image.
    pub(crate) fn create_overlay_pipeline(&mut self) -> Result<(), RenderError> {
        let device = self.device()?.clone();
        let push_constant_ranges = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::VERTEX)
            .offset(0)
            .size(OVERLAY_PUSH_CONSTANTS_SIZE as u32)];
        let layout_info =
            vk::PipelineLayoutCreateInfo::default().push_constant_ranges(&push_constant_ranges);
        // SAFETY: the layout has a single small vertex push-constant range and no descriptor sets.
        let layout = unsafe { device.create_pipeline_layout(&layout_info, None) }?;
        self.overlay_pipeline_layout = Some(layout);

        let vertex_module = ShaderModule::new(&device, OVERLAY_VERTEX_SHADER)?;
        let fragment_module = ShaderModule::new(&device, OVERLAY_FRAGMENT_SHADER)?;
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
        let vertex_bindings = [vk::VertexInputBindingDescription::default()
            .binding(0)
            .stride(size_of::<OverlayVertex>() as u32)
            .input_rate(vk::VertexInputRate::VERTEX)];
        let vertex_attributes = [
            vk::VertexInputAttributeDescription::default()
                .location(0)
                .binding(0)
                .format(vk::Format::R32G32_SFLOAT)
                .offset(std::mem::offset_of!(OverlayVertex, position) as u32),
            vk::VertexInputAttributeDescription::default()
                .location(1)
                .binding(0)
                .format(vk::Format::R32G32B32A32_SFLOAT)
                .offset(std::mem::offset_of!(OverlayVertex, color) as u32),
        ];
        let vertex_input = vk::PipelineVertexInputStateCreateInfo::default()
            .vertex_binding_descriptions(&vertex_bindings)
            .vertex_attribute_descriptions(&vertex_attributes);
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
        let blend_attachments = [vk::PipelineColorBlendAttachmentState::default()
            .blend_enable(true)
            .src_color_blend_factor(vk::BlendFactor::SRC_ALPHA)
            .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .color_blend_op(vk::BlendOp::ADD)
            .src_alpha_blend_factor(vk::BlendFactor::ONE)
            .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .alpha_blend_op(vk::BlendOp::ADD)
            .color_write_mask(vk::ColorComponentFlags::RGBA)];
        let color_blend =
            vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend_attachments);
        let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dynamic_state =
            vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
        let color_formats = [self.surface_format.format];
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
        // SAFETY: shader modules, layout and the surface color format are valid for this device.
        let pipelines = unsafe {
            device.create_graphics_pipelines(self.pipeline_cache()?, &pipeline_info, None)
        }
        .map_err(|(_, error)| RenderError::Vulkan(error))?;
        self.overlay_pipeline = pipelines.into_iter().next();
        Ok(())
    }

    /// Pipeline UI: индексированные треугольники с предумноженной альфой прямо в swapchain image.
    pub(crate) fn create_ui_pipeline(&mut self) -> Result<(), RenderError> {
        let device = self.device()?.clone();
        let sampler_info = vk::SamplerCreateInfo::default()
            .mag_filter(vk::Filter::LINEAR)
            .min_filter(vk::Filter::LINEAR)
            .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .max_lod(0.0);
        // SAFETY: the sampler uses core linear filtering and clamp-to-edge addressing.
        self.ui_sampler = Some(unsafe { device.create_sampler(&sampler_info, None) }?);

        let push_constant_ranges = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT)
            .offset(0)
            .size(UI_PUSH_CONSTANTS_SIZE as u32)];
        let set_layouts = [self.texture_set_layout()?];
        let layout_info = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(&set_layouts)
            .push_constant_ranges(&push_constant_ranges);
        // SAFETY: the set layout is alive and the push-constant range fits in the guaranteed 128 bytes.
        let layout = unsafe { device.create_pipeline_layout(&layout_info, None) }?;
        self.ui_pipeline_layout = Some(layout);
        self.ui_pipeline = self.create_ui_graphics_pipeline(layout, self.surface_format.format)?;
        self.viewport_ui_pipeline = self.create_ui_graphics_pipeline(layout, LDR_FORMAT)?;
        Ok(())
    }

    /// Создаёт графический пайплайн UI для формата цвета; layout общий для swapchain и viewport.
    pub(crate) fn create_ui_graphics_pipeline(
        &self,
        layout: vk::PipelineLayout,
        color_format: vk::Format,
    ) -> Result<Option<vk::Pipeline>, RenderError> {
        let device = self.device()?.clone();
        let vertex_module = ShaderModule::new(&device, UI_VERTEX_SHADER)?;
        let fragment_module = ShaderModule::new(&device, UI_FRAGMENT_SHADER)?;
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
        let vertex_bindings = [vk::VertexInputBindingDescription::default()
            .binding(0)
            .stride(size_of::<UiVertex>() as u32)
            .input_rate(vk::VertexInputRate::VERTEX)];
        let vertex_attributes = [
            vk::VertexInputAttributeDescription::default()
                .location(0)
                .binding(0)
                .format(vk::Format::R32G32_SFLOAT)
                .offset(std::mem::offset_of!(UiVertex, position) as u32),
            vk::VertexInputAttributeDescription::default()
                .location(1)
                .binding(0)
                .format(vk::Format::R32G32_SFLOAT)
                .offset(std::mem::offset_of!(UiVertex, uv) as u32),
            vk::VertexInputAttributeDescription::default()
                .location(2)
                .binding(0)
                .format(vk::Format::R8G8B8A8_UNORM)
                .offset(std::mem::offset_of!(UiVertex, color) as u32),
        ];
        let vertex_input = vk::PipelineVertexInputStateCreateInfo::default()
            .vertex_binding_descriptions(&vertex_bindings)
            .vertex_attribute_descriptions(&vertex_attributes);
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
        let blend_attachments = [vk::PipelineColorBlendAttachmentState::default()
            .blend_enable(true)
            .src_color_blend_factor(vk::BlendFactor::ONE)
            .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .color_blend_op(vk::BlendOp::ADD)
            .src_alpha_blend_factor(vk::BlendFactor::ONE)
            .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .alpha_blend_op(vk::BlendOp::ADD)
            .color_write_mask(vk::ColorComponentFlags::RGBA)];
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
        // SAFETY: shader modules, layout and the color format are valid for this device.
        let pipelines = unsafe {
            device.create_graphics_pipelines(self.pipeline_cache()?, &pipeline_info, None)
        }
        .map_err(|(_, error)| RenderError::Vulkan(error))?;
        Ok(pipelines.into_iter().next())
    }
}
