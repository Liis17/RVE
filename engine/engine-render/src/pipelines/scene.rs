//! Конвейеры сцены: меши, surface-шейдеры, тени, skybox, кластеры и скиннинг.

use crate::*;

impl Renderer {
    pub(crate) fn create_cluster_pipeline(&mut self) -> Result<(), RenderError> {
        let device = self.device()?.clone();
        let cache = self.pipeline_cache()?;
        let set_layouts = [self.frame_set_layout()?];
        let layout_info = vk::PipelineLayoutCreateInfo::default().set_layouts(&set_layouts);
        let layout = unsafe { device.create_pipeline_layout(&layout_info, None) }?;
        self.cluster_pipeline_layout = Some(layout);
        self.cluster_pipeline = Some(create_compute_pipeline(
            &device,
            cache,
            layout,
            CLUSTER_COMPUTE_SHADER,
        )?);
        Ok(())
    }

    /// Compute-пайплайн skinning: четыре storage-буфера в наборе 0 и `vertex_count` в push constant.
    pub(crate) fn create_skin_pipeline(&mut self) -> Result<(), RenderError> {
        let device = self.device()?.clone();
        let cache = self.pipeline_cache()?;
        let bindings: [vk::DescriptorSetLayoutBinding<'_>; 4] = std::array::from_fn(|binding| {
            vk::DescriptorSetLayoutBinding::default()
                .binding(binding as u32)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE)
        });
        let set_info = vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings);
        // SAFETY: the layout describes four storage buffers used only by the skin compute shader.
        let set_layout = unsafe { device.create_descriptor_set_layout(&set_info, None) }?;
        self.skin_set_layout = Some(set_layout);
        let push_ranges = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::COMPUTE)
            .offset(0)
            .size(size_of::<u32>() as u32)];
        let set_layouts = [set_layout];
        let layout_info = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(&set_layouts)
            .push_constant_ranges(&push_ranges);
        // SAFETY: the set layout and push constant range are valid for this pipeline layout.
        let layout = unsafe { device.create_pipeline_layout(&layout_info, None) }?;
        self.skin_pipeline_layout = Some(layout);
        self.skin_pipeline = Some(create_compute_pipeline(
            &device,
            cache,
            layout,
            SKIN_COMPUTE_SHADER,
        )?);
        Ok(())
    }

    pub(crate) fn create_mesh_pipeline(&mut self) -> Result<(), RenderError> {
        let device = self.device()?.clone();
        let push_constant_ranges = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT)
            .offset(0)
            .size(PUSH_CONSTANTS_SIZE as u32)];
        let set_layouts = [
            self.frame_set_layout()?,
            self.material_set_layout()?,
            self.environment_set_layout
                .ok_or(RenderError::InvalidState(
                    "environment descriptor layout отсутствует",
                ))?,
        ];
        let layout_info = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(&set_layouts)
            .push_constant_ranges(&push_constant_ranges);
        // SAFETY: device selection checks the required push-constant range before device creation.
        self.pipeline_layout = Some(unsafe { device.create_pipeline_layout(&layout_info, None) }?);

        let vertex_module = ShaderModule::new(&device, MESH_VERTEX_SHADER)?;
        let opaque_module = ShaderModule::new(&device, MESH_OPAQUE_FRAGMENT_SHADER)?;
        let mask_module = ShaderModule::new(&device, MESH_MASK_FRAGMENT_SHADER)?;
        let blend_module = ShaderModule::new(&device, MESH_BLEND_FRAGMENT_SHADER)?;
        let wireframe_module = ShaderModule::new(&device, WIREFRAME_FRAGMENT_SHADER)?;
        let vertex_stage = vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::VERTEX)
            .module(vertex_module.handle)
            .name(c"main");
        let opaque_stages = [
            vertex_stage,
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(opaque_module.handle)
                .name(c"main"),
        ];
        let mask_stages = [
            vertex_stage,
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(mask_module.handle)
                .name(c"main"),
        ];
        let blend_stages = [
            vertex_stage,
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(blend_module.handle)
                .name(c"main"),
        ];
        let wireframe_stages = [
            vertex_stage,
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(wireframe_module.handle)
                .name(c"main"),
        ];
        // Binding 1 — позиции прошлого кадра для motion vectors; статический меш подаёт тот же буфер.
        let vertex_bindings = [
            vk::VertexInputBindingDescription::default()
                .binding(0)
                .stride(size_of::<Vertex>() as u32)
                .input_rate(vk::VertexInputRate::VERTEX),
            vk::VertexInputBindingDescription::default()
                .binding(1)
                .stride(size_of::<Vertex>() as u32)
                .input_rate(vk::VertexInputRate::VERTEX),
            instance_binding(MESH_INSTANCE_BINDING),
        ];
        let vertex_attributes = [
            vk::VertexInputAttributeDescription::default()
                .location(0)
                .binding(0)
                .format(vk::Format::R32G32B32_SFLOAT)
                .offset(std::mem::offset_of!(Vertex, position) as u32),
            vk::VertexInputAttributeDescription::default()
                .location(1)
                .binding(0)
                .format(vk::Format::R32G32B32_SFLOAT)
                .offset(std::mem::offset_of!(Vertex, normal) as u32),
            vk::VertexInputAttributeDescription::default()
                .location(2)
                .binding(0)
                .format(vk::Format::R32G32_SFLOAT)
                .offset(std::mem::offset_of!(Vertex, uv) as u32),
            vk::VertexInputAttributeDescription::default()
                .location(3)
                .binding(0)
                .format(vk::Format::R32G32B32_SFLOAT)
                .offset(std::mem::offset_of!(Vertex, color) as u32),
            vk::VertexInputAttributeDescription::default()
                .location(4)
                .binding(1)
                .format(vk::Format::R32G32B32_SFLOAT)
                .offset(std::mem::offset_of!(Vertex, position) as u32),
        ]
        .into_iter()
        .chain(instance_attributes(
            MESH_INSTANCE_BINDING,
            5,
            MESH_INSTANCE_VECTORS,
        ))
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
            .polygon_mode(vk::PolygonMode::FILL)
            .cull_mode(vk::CullModeFlags::BACK)
            .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
            .line_width(1.0);
        let multisample = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(vk::SampleCountFlags::TYPE_1);
        let velocity_write = vk::PipelineColorBlendAttachmentState::default()
            .color_write_mask(vk::ColorComponentFlags::R | vk::ColorComponentFlags::G);
        let blend_attachments = [
            vk::PipelineColorBlendAttachmentState::default()
                .color_write_mask(vk::ColorComponentFlags::RGBA),
            velocity_write,
        ];
        let color_blend =
            vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend_attachments);
        let wireframe_attachments = [
            vk::PipelineColorBlendAttachmentState::default()
                .color_write_mask(vk::ColorComponentFlags::RGBA),
            vk::PipelineColorBlendAttachmentState::default()
                .color_write_mask(vk::ColorComponentFlags::empty()),
        ];
        let wireframe_color_blend =
            vk::PipelineColorBlendStateCreateInfo::default().attachments(&wireframe_attachments);
        let transparent_attachments = [
            vk::PipelineColorBlendAttachmentState::default()
                .blend_enable(true)
                .src_color_blend_factor(vk::BlendFactor::SRC_ALPHA)
                .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
                .color_blend_op(vk::BlendOp::ADD)
                .color_write_mask(
                    vk::ColorComponentFlags::R
                        | vk::ColorComponentFlags::G
                        | vk::ColorComponentFlags::B,
                ),
            velocity_write,
        ];
        let transparent_color_blend =
            vk::PipelineColorBlendStateCreateInfo::default().attachments(&transparent_attachments);
        let overdraw_attachments = [
            vk::PipelineColorBlendAttachmentState::default()
                .blend_enable(true)
                .src_color_blend_factor(vk::BlendFactor::ONE)
                .dst_color_blend_factor(vk::BlendFactor::ONE)
                .color_blend_op(vk::BlendOp::ADD)
                .color_write_mask(vk::ColorComponentFlags::R),
            vk::PipelineColorBlendAttachmentState::default()
                .color_write_mask(vk::ColorComponentFlags::empty()),
        ];
        let overdraw_color_blend =
            vk::PipelineColorBlendStateCreateInfo::default().attachments(&overdraw_attachments);
        let depth_stencil = vk::PipelineDepthStencilStateCreateInfo::default()
            .depth_test_enable(true)
            .depth_write_enable(true)
            .depth_compare_op(vk::CompareOp::LESS);
        let transparent_depth_stencil = vk::PipelineDepthStencilStateCreateInfo::default()
            .depth_test_enable(true)
            .depth_write_enable(false)
            .depth_compare_op(vk::CompareOp::LESS);
        let overdraw_depth_stencil = vk::PipelineDepthStencilStateCreateInfo::default()
            .depth_test_enable(false)
            .depth_write_enable(false);
        let opaque_equal_depth_stencil = vk::PipelineDepthStencilStateCreateInfo::default()
            .depth_test_enable(true)
            .depth_write_enable(true)
            .depth_compare_op(vk::CompareOp::LESS_OR_EQUAL);
        let prepass_depth_stencil = vk::PipelineDepthStencilStateCreateInfo::default()
            .depth_test_enable(true)
            .depth_write_enable(true)
            .depth_compare_op(vk::CompareOp::LESS);
        let prepass_stages = [vertex_stage];
        let no_color_blend = vk::PipelineColorBlendStateCreateInfo::default();
        let wireframe_rasterization = vk::PipelineRasterizationStateCreateInfo::default()
            .depth_bias_enable(true)
            .depth_bias_constant_factor(-1.0)
            .depth_bias_slope_factor(-1.0)
            .polygon_mode(vk::PolygonMode::LINE)
            .cull_mode(vk::CullModeFlags::BACK)
            .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
            .line_width(1.0);
        let wireframe_depth_stencil = vk::PipelineDepthStencilStateCreateInfo::default()
            .depth_test_enable(true)
            .depth_write_enable(false)
            .depth_compare_op(vk::CompareOp::LESS_OR_EQUAL);
        let dynamic_states = [
            vk::DynamicState::VIEWPORT,
            vk::DynamicState::SCISSOR,
            vk::DynamicState::CULL_MODE,
            vk::DynamicState::FRONT_FACE,
        ];
        let dynamic_state =
            vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
        let color_formats = [OFFSCREEN_FORMAT, VELOCITY_FORMAT];
        let mut opaque_rendering_info = vk::PipelineRenderingCreateInfo::default()
            .color_attachment_formats(&color_formats)
            .depth_attachment_format(DEPTH_FORMAT);
        let mut mask_rendering_info = vk::PipelineRenderingCreateInfo::default()
            .color_attachment_formats(&color_formats)
            .depth_attachment_format(DEPTH_FORMAT);
        let mut blend_rendering_info = vk::PipelineRenderingCreateInfo::default()
            .color_attachment_formats(&color_formats)
            .depth_attachment_format(DEPTH_FORMAT);
        let mut overdraw_rendering_info = vk::PipelineRenderingCreateInfo::default()
            .color_attachment_formats(&color_formats)
            .depth_attachment_format(DEPTH_FORMAT);
        let mut wireframe_rendering_info = vk::PipelineRenderingCreateInfo::default()
            .color_attachment_formats(&color_formats)
            .depth_attachment_format(DEPTH_FORMAT);
        let mut opaque_equal_rendering_info = vk::PipelineRenderingCreateInfo::default()
            .color_attachment_formats(&color_formats)
            .depth_attachment_format(DEPTH_FORMAT);
        let mut prepass_rendering_info =
            vk::PipelineRenderingCreateInfo::default().depth_attachment_format(DEPTH_FORMAT);
        let pipeline_layout = self
            .pipeline_layout
            .ok_or(RenderError::InvalidState("pipeline layout отсутствует"))?;
        let pipeline_info = [
            vk::GraphicsPipelineCreateInfo::default()
                .stages(&opaque_stages)
                .vertex_input_state(&vertex_input)
                .input_assembly_state(&input_assembly)
                .viewport_state(&viewport_state)
                .rasterization_state(&rasterization)
                .multisample_state(&multisample)
                .depth_stencil_state(&depth_stencil)
                .color_blend_state(&color_blend)
                .dynamic_state(&dynamic_state)
                .layout(pipeline_layout)
                .push_next(&mut opaque_rendering_info),
            vk::GraphicsPipelineCreateInfo::default()
                .stages(&mask_stages)
                .vertex_input_state(&vertex_input)
                .input_assembly_state(&input_assembly)
                .viewport_state(&viewport_state)
                .rasterization_state(&rasterization)
                .multisample_state(&multisample)
                .depth_stencil_state(&depth_stencil)
                .color_blend_state(&color_blend)
                .dynamic_state(&dynamic_state)
                .layout(pipeline_layout)
                .push_next(&mut mask_rendering_info),
            vk::GraphicsPipelineCreateInfo::default()
                .stages(&blend_stages)
                .vertex_input_state(&vertex_input)
                .input_assembly_state(&input_assembly)
                .viewport_state(&viewport_state)
                .rasterization_state(&rasterization)
                .multisample_state(&multisample)
                .depth_stencil_state(&transparent_depth_stencil)
                .color_blend_state(&transparent_color_blend)
                .dynamic_state(&dynamic_state)
                .layout(pipeline_layout)
                .push_next(&mut blend_rendering_info),
            vk::GraphicsPipelineCreateInfo::default()
                .stages(&opaque_stages)
                .vertex_input_state(&vertex_input)
                .input_assembly_state(&input_assembly)
                .viewport_state(&viewport_state)
                .rasterization_state(&rasterization)
                .multisample_state(&multisample)
                .depth_stencil_state(&overdraw_depth_stencil)
                .color_blend_state(&overdraw_color_blend)
                .dynamic_state(&dynamic_state)
                .layout(pipeline_layout)
                .push_next(&mut overdraw_rendering_info),
            vk::GraphicsPipelineCreateInfo::default()
                .stages(&opaque_stages)
                .vertex_input_state(&vertex_input)
                .input_assembly_state(&input_assembly)
                .viewport_state(&viewport_state)
                .rasterization_state(&rasterization)
                .multisample_state(&multisample)
                .depth_stencil_state(&opaque_equal_depth_stencil)
                .color_blend_state(&color_blend)
                .dynamic_state(&dynamic_state)
                .layout(pipeline_layout)
                .push_next(&mut opaque_equal_rendering_info),
            vk::GraphicsPipelineCreateInfo::default()
                .stages(&prepass_stages)
                .vertex_input_state(&vertex_input)
                .input_assembly_state(&input_assembly)
                .viewport_state(&viewport_state)
                .rasterization_state(&rasterization)
                .multisample_state(&multisample)
                .depth_stencil_state(&prepass_depth_stencil)
                .color_blend_state(&no_color_blend)
                .dynamic_state(&dynamic_state)
                .layout(pipeline_layout)
                .push_next(&mut prepass_rendering_info),
        ];
        // SAFETY: shader modules, layout and dynamic-rendering formats are valid for this device.
        let mut pipelines = match unsafe {
            device.create_graphics_pipelines(self.pipeline_cache()?, &pipeline_info, None)
        } {
            Ok(pipelines) => pipelines,
            Err((pipelines, error)) => {
                for pipeline in pipelines {
                    // SAFETY: these partial pipeline handles were returned by this device.
                    unsafe { device.destroy_pipeline(pipeline, None) };
                }
                return Err(RenderError::Vulkan(error));
            }
        };
        if pipelines.len() != 6 {
            for pipeline in pipelines {
                // SAFETY: these pipeline handles were returned by this device.
                unsafe { device.destroy_pipeline(pipeline, None) };
            }
            return Err(RenderError::InvalidState(
                "mesh pipelines не созданы полностью",
            ));
        }
        self.pipeline = Some(pipelines.remove(0));
        self.mask_pipeline = Some(pipelines.remove(0));
        self.blend_pipeline = Some(pipelines.remove(0));
        self.overdraw_pipeline = Some(pipelines.remove(0));
        self.opaque_equal_pipeline = Some(pipelines.remove(0));
        self.prepass_pipeline = Some(pipelines.remove(0));
        if self.capabilities.wireframe {
            let wireframe_info = [vk::GraphicsPipelineCreateInfo::default()
                .stages(&wireframe_stages)
                .vertex_input_state(&vertex_input)
                .input_assembly_state(&input_assembly)
                .viewport_state(&viewport_state)
                .rasterization_state(&wireframe_rasterization)
                .multisample_state(&multisample)
                .depth_stencil_state(&wireframe_depth_stencil)
                .color_blend_state(&wireframe_color_blend)
                .dynamic_state(&dynamic_state)
                .layout(pipeline_layout)
                .push_next(&mut wireframe_rendering_info)];
            // SAFETY: the optional LINE pipeline uses the same live layout and dynamic-rendering formats.
            let mut wireframe_pipelines = match unsafe {
                device.create_graphics_pipelines(self.pipeline_cache()?, &wireframe_info, None)
            } {
                Ok(pipelines) => pipelines,
                Err((pipelines, error)) => {
                    for pipeline in pipelines {
                        // SAFETY: these partial pipeline handles were returned by this device.
                        unsafe { device.destroy_pipeline(pipeline, None) };
                    }
                    return Err(RenderError::Vulkan(error));
                }
            };
            self.wireframe_pipeline = wireframe_pipelines.pop();
            if self.wireframe_pipeline.is_none() {
                return Err(RenderError::InvalidState("wireframe pipeline не создан"));
            }
        }
        Ok(())
    }

    pub(crate) fn create_surface_pipelines(
        &self,
        compiled: &CompiledSurfaceShader,
    ) -> Result<SurfacePipelines, RenderError> {
        let device = self.device()?;
        let layout = self
            .pipeline_layout
            .ok_or(RenderError::InvalidState("pipeline layout отсутствует"))?;
        let vertex_module = ShaderModule::new(device, MESH_VERTEX_SHADER)?;
        let opaque_bytes = spirv_bytes(&compiled.opaque);
        let mask_bytes = spirv_bytes(&compiled.mask);
        let blend_bytes = spirv_bytes(&compiled.blend);
        let opaque_module = ShaderModule::new(device, &opaque_bytes)?;
        let mask_module = ShaderModule::new(device, &mask_bytes)?;
        let blend_module = ShaderModule::new(device, &blend_bytes)?;
        let vertex_stage = vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::VERTEX)
            .module(vertex_module.handle)
            .name(c"main");
        let stages = |module| {
            [
                vertex_stage,
                vk::PipelineShaderStageCreateInfo::default()
                    .stage(vk::ShaderStageFlags::FRAGMENT)
                    .module(module)
                    .name(c"main"),
            ]
        };
        let opaque_stages = stages(opaque_module.handle);
        let mask_stages = stages(mask_module.handle);
        let blend_stages = stages(blend_module.handle);
        let vertex_bindings = [
            vk::VertexInputBindingDescription::default()
                .binding(0)
                .stride(size_of::<Vertex>() as u32)
                .input_rate(vk::VertexInputRate::VERTEX),
            vk::VertexInputBindingDescription::default()
                .binding(1)
                .stride(size_of::<Vertex>() as u32)
                .input_rate(vk::VertexInputRate::VERTEX),
            instance_binding(MESH_INSTANCE_BINDING),
        ];
        let vertex_attributes = [
            vk::VertexInputAttributeDescription::default()
                .location(0)
                .binding(0)
                .format(vk::Format::R32G32B32_SFLOAT)
                .offset(std::mem::offset_of!(Vertex, position) as u32),
            vk::VertexInputAttributeDescription::default()
                .location(1)
                .binding(0)
                .format(vk::Format::R32G32B32_SFLOAT)
                .offset(std::mem::offset_of!(Vertex, normal) as u32),
            vk::VertexInputAttributeDescription::default()
                .location(2)
                .binding(0)
                .format(vk::Format::R32G32_SFLOAT)
                .offset(std::mem::offset_of!(Vertex, uv) as u32),
            vk::VertexInputAttributeDescription::default()
                .location(3)
                .binding(0)
                .format(vk::Format::R32G32B32_SFLOAT)
                .offset(std::mem::offset_of!(Vertex, color) as u32),
            vk::VertexInputAttributeDescription::default()
                .location(4)
                .binding(1)
                .format(vk::Format::R32G32B32_SFLOAT)
                .offset(std::mem::offset_of!(Vertex, position) as u32),
        ]
        .into_iter()
        .chain(instance_attributes(
            MESH_INSTANCE_BINDING,
            5,
            MESH_INSTANCE_VECTORS,
        ))
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
            .polygon_mode(vk::PolygonMode::FILL)
            .cull_mode(vk::CullModeFlags::BACK)
            .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
            .line_width(1.0);
        let multisample = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(vk::SampleCountFlags::TYPE_1);
        let velocity_write = vk::PipelineColorBlendAttachmentState::default()
            .color_write_mask(vk::ColorComponentFlags::R | vk::ColorComponentFlags::G);
        let opaque_attachments = [
            vk::PipelineColorBlendAttachmentState::default()
                .color_write_mask(vk::ColorComponentFlags::RGBA),
            velocity_write,
        ];
        let blend_attachments = [
            vk::PipelineColorBlendAttachmentState::default()
                .blend_enable(true)
                .src_color_blend_factor(vk::BlendFactor::SRC_ALPHA)
                .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
                .color_blend_op(vk::BlendOp::ADD)
                .color_write_mask(
                    vk::ColorComponentFlags::R
                        | vk::ColorComponentFlags::G
                        | vk::ColorComponentFlags::B,
                ),
            velocity_write,
        ];
        let opaque_blend =
            vk::PipelineColorBlendStateCreateInfo::default().attachments(&opaque_attachments);
        let transparent_blend =
            vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend_attachments);
        let opaque_depth = vk::PipelineDepthStencilStateCreateInfo::default()
            .depth_test_enable(true)
            .depth_write_enable(true)
            .depth_compare_op(vk::CompareOp::LESS);
        let equal_depth = vk::PipelineDepthStencilStateCreateInfo::default()
            .depth_test_enable(true)
            .depth_write_enable(true)
            .depth_compare_op(vk::CompareOp::LESS_OR_EQUAL);
        let blend_depth = vk::PipelineDepthStencilStateCreateInfo::default()
            .depth_test_enable(true)
            .depth_write_enable(false)
            .depth_compare_op(vk::CompareOp::LESS);
        let dynamic_states = [
            vk::DynamicState::VIEWPORT,
            vk::DynamicState::SCISSOR,
            vk::DynamicState::CULL_MODE,
            vk::DynamicState::FRONT_FACE,
        ];
        let dynamic_state =
            vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
        let color_formats = [OFFSCREEN_FORMAT, VELOCITY_FORMAT];
        let mut opaque_rendering = vk::PipelineRenderingCreateInfo::default()
            .color_attachment_formats(&color_formats)
            .depth_attachment_format(DEPTH_FORMAT);
        let mut equal_rendering = vk::PipelineRenderingCreateInfo::default()
            .color_attachment_formats(&color_formats)
            .depth_attachment_format(DEPTH_FORMAT);
        let mut mask_rendering = vk::PipelineRenderingCreateInfo::default()
            .color_attachment_formats(&color_formats)
            .depth_attachment_format(DEPTH_FORMAT);
        let mut blend_rendering = vk::PipelineRenderingCreateInfo::default()
            .color_attachment_formats(&color_formats)
            .depth_attachment_format(DEPTH_FORMAT);
        let create_info = [
            vk::GraphicsPipelineCreateInfo::default()
                .stages(&opaque_stages)
                .vertex_input_state(&vertex_input)
                .input_assembly_state(&input_assembly)
                .viewport_state(&viewport_state)
                .rasterization_state(&rasterization)
                .multisample_state(&multisample)
                .depth_stencil_state(&opaque_depth)
                .color_blend_state(&opaque_blend)
                .dynamic_state(&dynamic_state)
                .layout(layout)
                .push_next(&mut opaque_rendering),
            vk::GraphicsPipelineCreateInfo::default()
                .stages(&opaque_stages)
                .vertex_input_state(&vertex_input)
                .input_assembly_state(&input_assembly)
                .viewport_state(&viewport_state)
                .rasterization_state(&rasterization)
                .multisample_state(&multisample)
                .depth_stencil_state(&equal_depth)
                .color_blend_state(&opaque_blend)
                .dynamic_state(&dynamic_state)
                .layout(layout)
                .push_next(&mut equal_rendering),
            vk::GraphicsPipelineCreateInfo::default()
                .stages(&mask_stages)
                .vertex_input_state(&vertex_input)
                .input_assembly_state(&input_assembly)
                .viewport_state(&viewport_state)
                .rasterization_state(&rasterization)
                .multisample_state(&multisample)
                .depth_stencil_state(&opaque_depth)
                .color_blend_state(&opaque_blend)
                .dynamic_state(&dynamic_state)
                .layout(layout)
                .push_next(&mut mask_rendering),
            vk::GraphicsPipelineCreateInfo::default()
                .stages(&blend_stages)
                .vertex_input_state(&vertex_input)
                .input_assembly_state(&input_assembly)
                .viewport_state(&viewport_state)
                .rasterization_state(&rasterization)
                .multisample_state(&multisample)
                .depth_stencil_state(&blend_depth)
                .color_blend_state(&transparent_blend)
                .dynamic_state(&dynamic_state)
                .layout(layout)
                .push_next(&mut blend_rendering),
        ];
        // SAFETY: these pipelines reuse the validated mesh vertex interface and active render formats.
        let mut pipelines = match unsafe {
            device.create_graphics_pipelines(self.pipeline_cache()?, &create_info, None)
        } {
            Ok(pipelines) => pipelines,
            Err((pipelines, error)) => {
                for pipeline in pipelines {
                    unsafe { device.destroy_pipeline(pipeline, None) };
                }
                return Err(RenderError::Vulkan(error));
            }
        };
        if pipelines.len() != 4 {
            for pipeline in pipelines {
                unsafe { device.destroy_pipeline(pipeline, None) };
            }
            return Err(RenderError::InvalidState(
                "surface shader pipelines не созданы полностью",
            ));
        }
        Ok(SurfacePipelines {
            opaque: pipelines.remove(0),
            opaque_equal: pipelines.remove(0),
            mask: pipelines.remove(0),
            blend: pipelines.remove(0),
        })
    }

    pub(crate) fn create_skybox_pipeline(&mut self) -> Result<(), RenderError> {
        let device = self.device()?.clone();
        let cache = self.pipeline_cache()?;
        let layout = self
            .pipeline_layout
            .ok_or(RenderError::InvalidState("pipeline layout отсутствует"))?;
        self.skybox_pipeline = Some(create_skybox_graphics_pipeline(&device, cache, layout)?);
        Ok(())
    }

    /// Pipeline для depth-only shadow pass с фиксированным depth bias.
    pub(crate) fn create_shadow_pipeline(&mut self) -> Result<(), RenderError> {
        let device = self.device()?.clone();
        let cache = self.pipeline_cache()?;
        let layout = self
            .pipeline_layout
            .ok_or(RenderError::InvalidState("pipeline layout отсутствует"))?;
        self.shadow_pipeline = Some(create_shadow_graphics_pipeline(
            &device,
            cache,
            layout,
            SHADOW_VERTEX_SHADER,
        )?);
        Ok(())
    }

    pub(crate) fn create_local_shadow_pipeline(&mut self) -> Result<(), RenderError> {
        let device = self.device()?.clone();
        let cache = self.pipeline_cache()?;
        let layout = self
            .pipeline_layout
            .ok_or(RenderError::InvalidState("pipeline layout отсутствует"))?;
        self.local_shadow_pipeline = Some(create_shadow_graphics_pipeline(
            &device,
            cache,
            layout,
            LOCAL_SHADOW_VERTEX_SHADER,
        )?);
        Ok(())
    }
}
